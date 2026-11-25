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
use anyhow::Context;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, Debug)]
struct AvailabilityRequest {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug)]
struct PictureRequest {
    max_size: usize,
    max_pictures: usize,
}

type ParsedRequest = (
    Vec<String>,
    usize,
    Option<AvailabilityRequest>,
    Option<PictureRequest>,
);

fn picture_value<'a>(
    picture: Option<&'a [u8]>,
    options: PictureRequest,
    pictures_sent: &mut usize,
) -> (&'static str, Option<&'a [u8]>) {
    match picture {
        None => ("173", None),
        Some(bytes) if bytes.len() > options.max_size => ("174", None),
        Some(_) if *pictures_sent >= options.max_pictures => ("175", None),
        Some(bytes) => {
            *pictures_sent += 1;
            ("1", Some(bytes))
        }
    }
}

const FREE_BUSY_SLOT_NANOS: i64 = 30 * 60 * 1_000_000_000;

impl AvailabilityRequest {
    fn parse(document: &Document) -> Result<Self, &'static str> {
        let fields = eas::child_elements(document);
        let mut start = None;
        let mut end = None;
        for field in fields {
            let Some(Node::Start(token)) = field.nodes.first() else {
                return Err("5");
            };
            let value = eas::find_text_after(&field, *token).ok_or("5")?;
            if is(&field, rr::START_TIME) {
                if start.is_some() {
                    return Err("5");
                }
                start = Some(parse_eas_time(value).ok_or("5")?);
            } else if is(&field, rr::END_TIME) {
                if end.is_some() {
                    return Err("5");
                }
                end = Some(parse_eas_time(value).ok_or("5")?);
            } else {
                return Err("5");
            }
        }
        let start = start.ok_or("5")?;
        let end = end.unwrap_or(start + Duration::days(7));
        let span = end.signed_duration_since(start);
        if span < Duration::minutes(30) || span > Duration::days(365) {
            return Err("5");
        }
        Ok(Self { start, end })
    }

    fn jmap_start(self) -> String {
        self.start
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn jmap_end(self) -> String {
        self.end
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }
}

fn parse_eas_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn merged_free_busy(
    request: AvailabilityRequest,
    periods: &[serde_json::Value],
) -> anyhow::Result<String> {
    let nanos = request
        .end
        .signed_duration_since(request.start)
        .num_nanoseconds()
        .context("free/busy range overflow")?;
    let slots = ((nanos + FREE_BUSY_SLOT_NANOS - 1) / FREE_BUSY_SLOT_NANOS) as usize;
    anyhow::ensure!(slots <= 365 * 48, "free/busy slot count exceeds limit");
    let mut values = vec![b'0'; slots];
    for period in periods {
        let start = period["utcStart"]
            .as_str()
            .and_then(parse_eas_time)
            .context("free/busy period has invalid start")?;
        let end = period["utcEnd"]
            .as_str()
            .and_then(parse_eas_time)
            .context("free/busy period has invalid end")?;
        if end <= start {
            continue;
        }
        let status = match period["event"]["freeBusyStatus"].as_str() {
            Some("x-eas-out-of-office") => b'3',
            _ => match period["busyStatus"].as_str() {
                Some("tentative") => b'1',
                Some("confirmed" | "unavailable") => b'2',
                _ => b'4',
            },
        };
        let clipped_start = start.max(request.start);
        let clipped_end = end.min(request.end);
        if clipped_end <= clipped_start {
            continue;
        }
        let first = clipped_start
            .signed_duration_since(request.start)
            .num_nanoseconds()
            .context("free/busy start offset overflow")?
            .div_euclid(FREE_BUSY_SLOT_NANOS) as usize;
        let last = clipped_end
            .signed_duration_since(request.start)
            .num_nanoseconds()
            .context("free/busy end offset overflow")?
            .checked_add(FREE_BUSY_SLOT_NANOS - 1)
            .context("free/busy range overflow")?
            / FREE_BUSY_SLOT_NANOS;
        let last = last as usize;
        for value in values
            .iter_mut()
            .take(last.min(slots))
            .skip(first.min(slots))
        {
            *value = (*value).max(status);
        }
    }
    String::from_utf8(values).context("free/busy stream is not UTF-8")
}

fn is(doc: &Document, expected: Token) -> bool {
    matches!(doc.nodes.first(), Some(Node::Start(token)) if token.code_page == expected.code_page && token.token == expected.token)
}

#[derive(Clone)]
struct Recipient {
    name: String,
    email: String,
    alias: String,
    kind: &'static str,
    principal_id: Option<String>,
    picture: Option<Vec<u8>>,
}

fn insert_recipient(candidates: &mut BTreeMap<String, Recipient>, recipient: Recipient) {
    use std::collections::btree_map::Entry;
    match candidates.entry(recipient.email.to_lowercase()) {
        Entry::Vacant(entry) => {
            entry.insert(recipient);
        }
        Entry::Occupied(mut entry) => {
            if entry.get().picture.is_none() {
                entry.get_mut().picture = recipient.picture;
            }
        }
    }
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

fn request(doc: &Document) -> Result<ParsedRequest, &'static str> {
    if !is(doc, rr::RESOLVE_RECIPIENTS) {
        return Err("5");
    }
    let mut queries = Vec::new();
    let mut max = 100usize;
    let mut options_seen = false;
    let mut availability = None;
    let mut picture = None;
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
                } else if is(&option, rr::AVAILABILITY) {
                    if availability.is_some() {
                        return Err("5");
                    }
                    availability = Some(AvailabilityRequest::parse(&option)?);
                } else if is(&option, rr::PICTURE) {
                    let mut max_size = 102400usize;
                    // MaxPictures is optional and limits each To query, not the
                    // complete ResolveRecipients request.
                    let mut max_pictures = usize::MAX;
                    let mut picture_fields = std::collections::BTreeSet::new();
                    for field in eas::child_elements(&option) {
                        let Some(Node::Start(token)) = field.nodes.first() else {
                            return Err("5");
                        };
                        if !picture_fields.insert((token.code_page, token.token)) {
                            return Err("5");
                        }
                        let value_text = eas::find_text_after(&field, *token).ok_or("5")?;
                        if is(&field, rr::MAX_SIZE) {
                            let value: usize = value_text.parse().map_err(|_| "5")?;
                            if value > 102400 {
                                return Err("5");
                            }
                            max_size = value;
                        } else if is(&field, rr::MAX_PICTURES) {
                            // MS-ASDTYPE defines integer here as xs:unsignedInt;
                            // parsing as usize would accept values outside the
                            // wire schema on 64-bit hosts.
                            max_pictures = value_text.parse::<u32>().map_err(|_| "5")? as usize;
                        } else {
                            return Err("5");
                        }
                    }
                    picture = Some(PictureRequest {
                        max_size,
                        max_pictures,
                    });
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
    Ok((queries, max, availability, picture))
}

pub(super) async fn handle(
    state: &AppState,
    auth: &AuthenticatedSession,
    document: &Document,
    version: super::ProtocolVersion,
) -> Response {
    let parsed = request(document).and_then(|(queries, max, availability, picture)| {
        if picture.is_some() && version == super::ProtocolVersion::V14_0 {
            Err("6")
        } else {
            Ok((queries, max, availability, picture))
        }
    });
    let mut b = DocumentBuilder::new();
    b.start(rr::RESOLVE_RECIPIENTS);
    let (queries, max, availability, picture) = match parsed {
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
                    insert_recipient(
                        &mut candidates,
                        Recipient {
                            name: p.display_name,
                            email,
                            alias: p.alias,
                            kind: "1",
                            principal_id: Some(p.id),
                            picture: None,
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
                .search_contacts_for_recipients(auth, &queries)
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
                    insert_recipient(
                        &mut candidates,
                        Recipient {
                            name: if name.is_empty() {
                                email.clone()
                            } else {
                                name.clone()
                            },
                            email,
                            alias: String::new(),
                            kind: "2",
                            principal_id: None,
                            picture: c.picture.clone(),
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
    let matches: Vec<_> = queries
        .iter()
        .map(|query| matching(query, &candidates))
        .collect();
    let principal_ids: Vec<_> = if availability.is_some() {
        matches
            .iter()
            .filter(|items| items.len() == 1)
            .filter_map(|items| items[0].principal_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    let availability_results = if let Some(request) = availability {
        match state
            .jmap
            .principal_availabilities(
                auth,
                &principal_ids,
                &request.jmap_start(),
                &request.jmap_end(),
            )
            .await
        {
            Ok(results) => results,
            Err(error) => {
                tracing::warn!(%error, "recipient availability request failed");
                principal_ids
                    .iter()
                    .cloned()
                    .map(|id| (id, Err("162")))
                    .collect()
            }
        }
    } else {
        HashMap::new()
    };
    b.leaf(rr::STATUS, "1");
    for (query, items) in queries.into_iter().zip(matches) {
        let mut pictures_sent = 0usize;
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
            if let Some(options) = picture {
                b.start(rr::PICTURE);
                let (status, data) =
                    picture_value(item.picture.as_deref(), options, &mut pictures_sent);
                b.leaf(rr::STATUS, status);
                if let Some(bytes) = data {
                    use base64::Engine;
                    b.leaf(
                        rr::DATA,
                        base64::engine::general_purpose::STANDARD.encode(bytes),
                    );
                }
                b.end();
            }
            if let Some(request) = availability.filter(|_| count == 1) {
                b.start(rr::AVAILABILITY);
                let result = item
                    .principal_id
                    .as_ref()
                    .and_then(|id| availability_results.get(id));
                match result {
                    Some(Ok(periods)) => match merged_free_busy(request, periods) {
                        Ok(stream) => {
                            b.leaf(rr::STATUS, "1");
                            b.leaf(rr::MERGED_FREE_BUSY, stream);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "invalid recipient availability response");
                            b.leaf(rr::STATUS, "162");
                        }
                    },
                    Some(Err(status)) => b.leaf(rr::STATUS, *status),
                    None => b.leaf(rr::STATUS, "163"),
                }
                b.end();
            }
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
                principal_id: Some("principal-1".into()),
                picture: None,
            },
            Recipient {
                name: "Alice Smith".into(),
                email: "smith@example.test".into(),
                alias: String::new(),
                kind: "2",
                principal_id: None,
                picture: None,
            },
        ];
        assert_eq!(matching("ALICE", &recipients).len(), 1);
        assert_eq!(matching("ali", &recipients).len(), 2);
        assert!(matching("unknown", &recipients).is_empty());
    }

    #[test]
    fn duplicate_contact_and_principal_address_resolves_once_to_principal() {
        let mut candidates = BTreeMap::new();
        insert_recipient(
            &mut candidates,
            Recipient {
                name: "Directory person".into(),
                email: "person@example.test".into(),
                alias: "person".into(),
                kind: "1",
                principal_id: Some("principal-1".into()),
                picture: None,
            },
        );
        insert_recipient(
            &mut candidates,
            Recipient {
                name: "Saved contact".into(),
                email: "PERSON@example.test".into(),
                alias: String::new(),
                kind: "2",
                principal_id: None,
                picture: Some(vec![1, 2, 3]),
            },
        );
        let candidates: Vec<_> = candidates.into_values().collect();
        let matches = matching("person@example.test", &candidates);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, "1");
        assert_eq!(matches[0].principal_id.as_deref(), Some("principal-1"));
        assert_eq!(matches[0].picture.as_deref(), Some(&[1, 2, 3][..]));
    }

    fn availability_document(start: &str, end: Option<&str>) -> Document {
        let mut builder = DocumentBuilder::new();
        builder.start(rr::AVAILABILITY);
        builder.leaf(rr::START_TIME, start);
        if let Some(end) = end {
            builder.leaf(rr::END_TIME, end);
        }
        builder.end();
        builder.end();
        wbxml::decode_document(&wbxml::encode_document(&builder.finish())).unwrap()
    }

    #[test]
    fn availability_option_requires_start_and_rejects_duplicate_ranges() {
        let mut builder = DocumentBuilder::new();
        builder.start(rr::RESOLVE_RECIPIENTS);
        builder.leaf(rr::TO, "person@example.test");
        builder.start(rr::OPTIONS);
        builder.start(rr::AVAILABILITY);
        builder.leaf(rr::START_TIME, "2042-05-05T10:00:00Z");
        builder.end();
        builder.end();
        builder.end();
        let document = wbxml::decode_document(&wbxml::encode_document(&builder.finish())).unwrap();
        let (_, _, availability, _) = request(&document).unwrap();
        let availability = availability.unwrap();
        assert_eq!(availability.end - availability.start, Duration::days(7));

        let mut builder = DocumentBuilder::new();
        builder.start(rr::RESOLVE_RECIPIENTS);
        builder.leaf(rr::TO, "person@example.test");
        builder.start(rr::OPTIONS);
        builder.start(rr::AVAILABILITY);
        builder.end();
        builder.end();
        builder.end();
        let document = wbxml::decode_document(&wbxml::encode_document(&builder.finish())).unwrap();
        assert_eq!(request(&document).unwrap_err(), "5");
    }

    #[test]
    fn picture_options_validate_size_and_picture_limit() {
        let mut builder = DocumentBuilder::new();
        builder.start(rr::RESOLVE_RECIPIENTS);
        builder.leaf(rr::TO, "person@example.test");
        builder.start(rr::OPTIONS);
        builder.start(rr::PICTURE);
        builder.leaf(rr::MAX_SIZE, "102400");
        builder.leaf(rr::MAX_PICTURES, "2");
        builder.end();
        builder.end();
        builder.end();
        let document = wbxml::decode_document(&wbxml::encode_document(&builder.finish())).unwrap();
        let (_, _, _, picture) = request(&document).unwrap();
        assert_eq!(picture.unwrap().max_size, 102400);
        assert_eq!(picture.unwrap().max_pictures, 2);

        let mut builder = DocumentBuilder::new();
        builder.start(rr::RESOLVE_RECIPIENTS);
        builder.leaf(rr::TO, "person@example.test");
        builder.start(rr::OPTIONS);
        builder.start(rr::PICTURE);
        builder.end();
        builder.end();
        builder.end();
        let document = wbxml::decode_document(&wbxml::encode_document(&builder.finish())).unwrap();
        let (_, _, _, picture) = request(&document).unwrap();
        assert_eq!(picture.unwrap().max_pictures, usize::MAX);

        for value in ["4294967295", "4294967296", "-1"] {
            let mut builder = DocumentBuilder::new();
            builder.start(rr::RESOLVE_RECIPIENTS);
            builder.leaf(rr::TO, "person@example.test");
            builder.start(rr::OPTIONS);
            builder.start(rr::PICTURE);
            builder.leaf(rr::MAX_PICTURES, value);
            builder.end();
            builder.end();
            builder.end();
            let document =
                wbxml::decode_document(&wbxml::encode_document(&builder.finish())).unwrap();
            if value == "4294967295" {
                assert_eq!(
                    request(&document).unwrap().3.unwrap().max_pictures,
                    u32::MAX as usize
                );
            } else {
                assert_eq!(request(&document).unwrap_err(), "5");
            }
        }

        let mut builder = DocumentBuilder::new();
        builder.start(rr::RESOLVE_RECIPIENTS);
        builder.leaf(rr::TO, "person@example.test");
        builder.start(rr::OPTIONS);
        builder.start(rr::PICTURE);
        builder.leaf(rr::MAX_SIZE, "102401");
        builder.end();
        builder.end();
        builder.end();
        let document = wbxml::decode_document(&wbxml::encode_document(&builder.finish())).unwrap();
        assert_eq!(request(&document).unwrap_err(), "5");
    }

    #[test]
    fn pictures_report_missing_oversized_and_count_limited_statuses() {
        let options = PictureRequest {
            max_size: 2,
            max_pictures: 1,
        };
        let mut sent = 0;
        assert_eq!(picture_value(None, options, &mut sent), ("173", None));
        assert_eq!(
            picture_value(Some(&[1, 2, 3]), options, &mut sent),
            ("174", None)
        );
        assert_eq!(
            picture_value(Some(&[1, 2]), options, &mut sent),
            ("1", Some(&[1, 2][..]))
        );
        assert_eq!(picture_value(Some(&[9]), options, &mut sent), ("175", None));
        assert_eq!(sent, 1);

        // Each To query gets its own MaxPictures budget.
        let mut second_query_sent = 0;
        assert_eq!(
            picture_value(Some(&[1]), options, &mut second_query_sent),
            ("1", Some(&[1][..]))
        );
    }

    #[test]
    fn availability_requires_valid_bounded_time_range() {
        let valid = availability_document("2042-05-05T10:00:00Z", None);
        let parsed = AvailabilityRequest::parse(&valid).unwrap();
        assert_eq!(parsed.end - parsed.start, Duration::days(7));

        for (start, end) in [
            ("invalid", Some("2042-05-05T11:00:00Z")),
            ("2042-05-05T10:00:00Z", Some("2042-05-05T10:29:59Z")),
            ("2042-05-05T10:00:00Z", Some("2043-05-06T10:00:00Z")),
        ] {
            assert!(AvailabilityRequest::parse(&availability_document(start, end)).is_err());
        }
    }

    #[test]
    fn availability_maps_busy_tentative_oof_and_free_into_half_hour_slots() {
        let request = AvailabilityRequest::parse(&availability_document(
            "2042-05-05T10:00:00Z",
            Some("2042-05-05T12:00:00Z"),
        ))
        .unwrap();
        let periods = vec![
            serde_json::json!({"utcStart":"2042-05-05T10:10:00Z","utcEnd":"2042-05-05T10:20:00Z","busyStatus":"confirmed"}),
            serde_json::json!({"utcStart":"2042-05-05T10:30:00Z","utcEnd":"2042-05-05T10:40:00Z","busyStatus":"tentative"}),
            serde_json::json!({"utcStart":"2042-05-05T11:00:00Z","utcEnd":"2042-05-05T11:10:00Z","busyStatus":"confirmed","event":{"freeBusyStatus":"x-eas-out-of-office"}}),
            serde_json::json!({"utcStart":"2042-05-05T11:30:00Z","utcEnd":"2042-05-05T11:30:00.500Z","busyStatus":"tentative"}),
            serde_json::json!({"utcStart":"2042-05-05T12:00:00Z","utcEnd":"2042-05-05T12:30:00Z","busyStatus":"confirmed"}),
        ];
        assert_eq!(merged_free_busy(request, &periods).unwrap(), "2131");
        assert_eq!(merged_free_busy(request, &[]).unwrap(), "0000");
    }
}
