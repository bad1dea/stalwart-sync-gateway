use crate::wbxml::{
    self,
    eas::{self, airsync as air, airsync_base as base, email, email2},
    token::Token,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

fn is_token(doc: &wbxml::Document, token: Token) -> bool {
    matches!(doc.nodes.first(), Some(wbxml::Node::Start(t)) if t.code_page == token.code_page && t.token == token.token)
}

fn text(doc: &wbxml::Document) -> Result<String, &'static str> {
    let mut value = String::new();
    for node in doc.nodes.iter().skip(1) {
        match node {
            wbxml::Node::Text(part) => value.push_str(part),
            wbxml::Node::End => {}
            _ => return Err("invalid draft text"),
        }
    }
    Ok(value)
}

pub(super) struct DraftCreate {
    pub properties: Value,
    pub mime: Option<Vec<u8>>,
    pub attachments: Vec<crate::model::DraftAttachment>,
}

pub(super) struct CreateOptions<'a> {
    pub marker: &'a str,
    pub content_marker: &'a str,
    pub is_draft: bool,
    pub read: Option<bool>,
    pub flag: Option<&'a crate::model::MailFlag>,
}

fn bytes(doc: &wbxml::Document) -> Result<Vec<u8>, &'static str> {
    let mut value = Vec::new();
    for node in doc.nodes.iter().skip(1) {
        match node {
            wbxml::Node::Text(part) => value.extend_from_slice(part.as_bytes()),
            wbxml::Node::Opaque(part) => value.extend_from_slice(part),
            wbxml::Node::End => {}
            _ => return Err("invalid draft body data"),
        }
    }
    Ok(value)
}

fn attachments(doc: &wbxml::Document) -> Result<Vec<crate::model::DraftAttachment>, &'static str> {
    let mut result = Vec::new();
    let mut ids = BTreeSet::new();
    for item in eas::child_elements(doc) {
        if !is_token(&item, base::ADD) {
            return Err("unsupported draft attachment operation");
        }
        let mut seen = BTreeSet::new();
        let mut client_id = None;
        let mut name = None;
        let mut content_type = None;
        let mut content = None;
        let mut content_id = None;
        let mut content_location = None;
        let mut inline = false;
        let mut method = None;
        for field in eas::child_elements(&item) {
            let Some(wbxml::Node::Start(token)) = field.nodes.first() else {
                return Err("invalid attachment field");
            };
            if !seen.insert((token.code_page, token.token)) {
                return Err("duplicate attachment field");
            }
            if is_token(&field, base::CLIENT_ID) {
                client_id = Some(text(&field)?);
            } else if is_token(&field, base::DISPLAY_NAME) {
                name = Some(text(&field)?);
            } else if is_token(&field, base::CONTENT_TYPE) {
                content_type = Some(text(&field)?);
            } else if is_token(&field, base::CONTENT_ID) {
                content_id = Some(text(&field)?);
            } else if is_token(&field, base::CONTENT_LOCATION) {
                content_location = Some(text(&field)?);
            } else if is_token(&field, base::METHOD) {
                method = text(&field)?.parse::<u8>().ok();
            } else if is_token(&field, base::IS_INLINE) {
                if token.has_content {
                    return Err("invalid inline marker");
                }
                inline = true;
            } else if is_token(&field, base::CONTENT) {
                if !field
                    .nodes
                    .iter()
                    .any(|node| matches!(node, wbxml::Node::Opaque(_)))
                    || field
                        .nodes
                        .iter()
                        .any(|node| matches!(node, wbxml::Node::Text(_)))
                {
                    return Err("attachment content must be opaque");
                }
                content = Some(bytes(&field)?);
            } else {
                return Err("unsupported attachment field");
            }
        }
        let client_id = client_id
            .filter(|value| !value.is_empty())
            .ok_or("missing attachment client id")?;
        if !ids.insert(client_id.clone()) {
            return Err("duplicate attachment client id");
        }
        let name = name
            .filter(|value| !value.is_empty())
            .ok_or("missing attachment name")?;
        let method = method
            .filter(|value| *value == 1 || *value == 5)
            .ok_or("invalid attachment method")?;
        let content_type = content_type.unwrap_or_else(|| {
            if method == 5 {
                "message/rfc822"
            } else {
                "application/octet-stream"
            }
            .to_owned()
        });
        if [
            Some(&name),
            Some(&content_type),
            content_id.as_ref(),
            content_location.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|value| value.chars().any(char::is_control))
        {
            return Err("invalid attachment metadata");
        }
        let embedded = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("message/rfc822");
        if (method == 5) != embedded {
            return Err("attachment method does not match content type");
        }
        result.push(crate::model::DraftAttachment {
            client_id,
            name,
            content_type,
            content: content.ok_or("missing attachment content")?,
            content_id,
            content_location,
            inline,
        });
    }
    Ok(result)
}

pub(super) fn create_properties(
    command: &wbxml::Document,
    folder: &str,
    sender: &str,
    options: CreateOptions<'_>,
) -> Result<DraftCreate, &'static str> {
    let children = eas::child_elements(command);
    for child in &children {
        if is_token(child, air::CLASS) {
            if text(child)? != "Email" {
                return Err("unsupported item class");
            }
        } else if !is_token(child, air::CLIENT_ID) && !is_token(child, air::APPLICATION_DATA) {
            return Err("unsupported draft command");
        }
    }
    let apps: Vec<_> = children
        .iter()
        .filter(|child| is_token(child, air::APPLICATION_DATA))
        .collect();
    if apps.len() != 1 {
        return Err("missing ApplicationData");
    }
    let mut properties = Map::new();
    let mut seen = BTreeSet::new();
    let mut body: Option<(u8, Vec<u8>)> = None;
    let mut draft_attachments = Vec::new();
    for field in eas::child_elements(apps[0]) {
        let Some(wbxml::Node::Start(token)) = field.nodes.first() else {
            return Err("invalid draft field");
        };
        if !seen.insert((token.code_page, token.token)) {
            return Err("duplicate draft field");
        }
        if is_token(&field, email::READ) {
            if !matches!(text(&field)?.as_str(), "0" | "1") {
                return Err("invalid draft read state");
            }
            continue;
        }
        if is_token(&field, email::FLAG) {
            continue;
        }
        if is_token(&field, base::ATTACHMENTS) {
            draft_attachments = attachments(&field)?;
        } else if is_token(&field, base::BODY) {
            let mut body_type = None;
            let mut data = None;
            let mut body_fields = BTreeSet::new();
            for part in eas::child_elements(&field) {
                let Some(wbxml::Node::Start(token)) = part.nodes.first() else {
                    return Err("invalid draft body field");
                };
                if !body_fields.insert((token.code_page, token.token)) {
                    return Err("duplicate draft body field");
                }
                if is_token(&part, base::TYPE) && body_type.is_none() {
                    body_type = text(&part)?.parse::<u8>().ok();
                } else if is_token(&part, base::DATA) && data.is_none() {
                    data = Some(bytes(&part)?);
                } else if is_token(&part, base::ESTIMATED_DATA_SIZE) {
                    text(&part)?
                        .parse::<u64>()
                        .map_err(|_| "invalid body size")?;
                } else if is_token(&part, base::TRUNCATED) {
                    if text(&part)? != "0" {
                        return Err("truncated draft body");
                    }
                } else if is_token(&part, base::PREVIEW) {
                    text(&part)?;
                } else {
                    return Err("unsupported draft body field");
                }
            }
            let kind = body_type.ok_or("missing draft body type")?;
            if kind != 1 && kind != 2 && kind != 4 {
                return Err("unsupported draft body type");
            }
            let data = data.unwrap_or_default();
            if kind == 4 && data.is_empty() {
                return Err("empty MIME draft");
            }
            body = Some((kind, data));
        } else if is_token(&field, email::SUBJECT) {
            let subject = text(&field)?;
            if subject.chars().any(char::is_control) {
                return Err("invalid draft subject");
            }
            properties.insert("subject".into(), json!(subject));
        } else if is_token(&field, email::IMPORTANCE) {
            let importance = match text(&field)?.as_str() {
                "0" => "low",
                "1" => "normal",
                "2" => "high",
                _ => return Err("invalid draft importance"),
            };
            properties.insert("header:Importance:asRaw".into(), json!(importance));
        } else {
            let header = if is_token(&field, email::TO) {
                "To"
            } else if is_token(&field, email::CC) {
                "Cc"
            } else if is_token(&field, email2::BCC) {
                "Bcc"
            } else if is_token(&field, email::REPLY_TO) {
                "Reply-To"
            } else {
                return Err("unsupported draft field");
            };
            let value = text(&field)?;
            if value.chars().any(|ch| ch.is_control() && ch != '\t') {
                return Err("invalid draft header");
            }
            if !value.is_empty() {
                properties.insert(format!("header:{header}:asRaw"), json!(value));
            }
        }
    }
    if body.as_ref().is_some_and(|(kind, _)| *kind == 4) && !draft_attachments.is_empty() {
        return Err("MIME draft attachments require editing the MIME body");
    }
    if body.as_ref().is_some_and(|(kind, _)| *kind == 4)
        && [
            email::TO,
            email::CC,
            email2::BCC,
            email::REPLY_TO,
            email::SUBJECT,
            email::IMPORTANCE,
        ]
        .iter()
        .any(|token| seen.contains(&(token.code_page, token.token)))
    {
        return Err("MIME drafts cannot include separate headers");
    }
    let mut keywords = Map::new();
    if options.is_draft {
        keywords.insert("$draft".into(), json!(true));
    }
    keywords.insert(options.marker.to_owned(), json!(true));
    keywords.insert(options.content_marker.to_owned(), json!(true));
    if options.read == Some(true) {
        keywords.insert("$seen".into(), json!(true));
    }
    if let Some(flag) = options.flag {
        if flag.status == 2 {
            keywords.insert("$flagged".into(), json!(true));
        }
        if flag.status != 0 {
            for key in flag.keywords().map_err(|_| "invalid flag metadata")? {
                keywords.insert(key, json!(true));
            }
        }
    }
    properties.insert("mailboxIds".into(), json!({folder:true}));
    properties.insert("keywords".into(), Value::Object(keywords));
    properties.insert("from".into(), json!([{"email":sender}]));
    let mut mime = None;
    if let Some((kind, value)) = body {
        if kind == 4 {
            mime = Some(value);
        } else {
            let value = String::from_utf8(value).map_err(|_| "invalid draft body encoding")?;
            properties.insert("bodyValues".into(), json!({"body":{"value":value}}));
            let part =
                json!([{"partId":"body", "type":if kind == 1 {"text/plain"} else {"text/html"}}]);
            properties.insert(if kind == 1 { "textBody" } else { "htmlBody" }.into(), part);
        }
    }
    Ok(DraftCreate {
        properties: Value::Object(properties),
        mime,
        attachments: draft_attachments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wbxml::eas::DocumentBuilder;

    fn draft_with(field: Token, value: &str) -> wbxml::Document {
        let mut b = DocumentBuilder::new();
        b.start(air::ADD);
        b.leaf(air::CLIENT_ID, "client");
        b.start(air::APPLICATION_DATA);
        b.leaf(field, value);
        b.end();
        b.end();
        b.finish()
    }

    #[test]
    fn rejects_header_injection_and_fields_not_yet_supported() {
        let parse = |doc: &wbxml::Document| {
            create_properties(
                doc,
                "drafts",
                "sender@example.test",
                CreateOptions {
                    marker: "identity",
                    content_marker: "content",
                    is_draft: true,
                    read: None,
                    flag: None,
                },
            )
        };
        assert!(parse(&draft_with(
            email::TO,
            "a@example.test\r\nBcc: hidden@example.test"
        ))
        .is_err());
        assert!(parse(&draft_with(email::IMPORTANCE, "9")).is_err());
        assert!(parse(&draft_with(email::TO, "a@example.test")).is_ok());
    }

    #[test]
    fn creates_non_draft_sync_items_in_the_requested_mailbox() {
        let item = create_properties(
            &draft_with(email::TO, "a@example.test"),
            "sent-mailbox",
            "sender@example.test",
            CreateOptions {
                marker: "identity",
                content_marker: "content",
                is_draft: false,
                read: None,
                flag: None,
            },
        )
        .unwrap();
        assert_eq!(item.properties["mailboxIds"]["sent-mailbox"], true);
        assert!(item.properties["keywords"]["$draft"].is_null());
        assert_eq!(item.properties["keywords"]["identity"], true);
        assert_eq!(item.properties["keywords"]["content"], true);
    }

    #[test]
    fn rejects_nonopaque_and_duplicate_attachments() {
        let build = |opaque: bool, duplicate: bool| {
            let mut b = DocumentBuilder::new();
            b.start(air::ADD);
            b.leaf(air::CLIENT_ID, "message");
            b.start(air::APPLICATION_DATA);
            b.start(base::ATTACHMENTS);
            for _ in 0..if duplicate { 2 } else { 1 } {
                b.start(base::ADD);
                b.leaf(base::CLIENT_ID, "attachment");
                b.leaf(base::METHOD, "1");
                b.leaf(base::DISPLAY_NAME, "file.bin");
                if opaque {
                    b.opaque_leaf(base::CONTENT, vec![0, 1, 255]);
                } else {
                    b.leaf(base::CONTENT, "not opaque");
                }
                b.end();
            }
            b.end();
            b.end();
            b.end();
            b.finish()
        };
        let parse = |doc: &wbxml::Document| {
            create_properties(
                doc,
                "drafts",
                "sender@example.test",
                CreateOptions {
                    marker: "identity",
                    content_marker: "content",
                    is_draft: true,
                    read: None,
                    flag: None,
                },
            )
        };
        assert!(parse(&build(false, false)).is_err());
        assert!(parse(&build(true, true)).is_err());
        assert!(parse(&build(true, false)).is_ok());
    }
}
