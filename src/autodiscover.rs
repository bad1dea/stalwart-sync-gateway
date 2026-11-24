use axum::{body::Bytes, extract::State, http::StatusCode, response::IntoResponse};
use serde::Deserialize;

use crate::http_server::AppState;

const MOBILE_SCHEMA: &str =
    "http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006";
const REQUEST_SCHEMA: &[u8] =
    b"http://schemas.microsoft.com/exchange/autodiscover/mobilesync/requestschema/2006";

#[derive(Deserialize)]
#[serde(rename = "Autodiscover")]
struct AutodiscoverRequest {
    #[serde(rename = "Request")]
    request: Request,
}

#[derive(Deserialize)]
struct Request {
    #[serde(rename = "EMailAddress")]
    email: String,
    #[serde(rename = "AcceptableResponseSchema")]
    schema: String,
}

fn valid_structure(text: &str) -> bool {
    use quick_xml::{events::Event, name::ResolveResult, NsReader};
    let mut reader = NsReader::from_str(text);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut root_seen = false;
    loop {
        let Ok((namespace, event)) = reader.read_resolved_event() else {
            return false;
        };
        match event {
            Event::Start(ref element) | Event::Empty(ref element) => {
                let name = element.local_name().as_ref().to_vec();
                let valid = matches!(namespace, ResolveResult::Bound(value) if value.as_ref() == REQUEST_SCHEMA)
                    && match stack.as_slice() {
                        [] => !root_seen && name == b"Autodiscover",
                        [root] => root == b"Autodiscover" && name == b"Request",
                        [root, request] => {
                            root == b"Autodiscover"
                                && request == b"Request"
                                && (name == b"EMailAddress" || name == b"AcceptableResponseSchema")
                        }
                        _ => false,
                    };
                if !valid {
                    return false;
                }
                if stack.is_empty() {
                    root_seen = true;
                }
                if matches!(event, Event::Start(_)) {
                    stack.push(name);
                }
            }
            Event::End(_) => {
                if stack.pop().is_none() {
                    return false;
                }
            }
            Event::DocType(_) => return false,
            Event::Text(text) if stack.len() < 3 && !text.iter().all(u8::is_ascii_whitespace) => {
                return false
            }
            Event::CData(_) if stack.len() != 3 => return false,
            Event::Eof => return root_seen && stack.is_empty(),
            _ => {}
        }
    }
}

fn response(body: &[u8], url: &str) -> String {
    let request = std::str::from_utf8(body)
        .ok()
        .filter(|text| text.len() <= 65536 && valid_structure(text))
        .and_then(|text| quick_xml::de::from_str::<AutodiscoverRequest>(text).ok());
    let error = match request.as_ref().map(|request| &request.request) {
        None => Some((600, "Invalid request")),
        Some(request)
            if request.email.trim().is_empty() || request.email.chars().any(char::is_control) =>
        {
            Some((600, "Invalid request"))
        }
        Some(request) if request.schema.trim() != MOBILE_SCHEMA => {
            Some((601, "Unsupported response schema"))
        }
        Some(_) => None,
    };
    if let Some((code, message)) = error {
        return format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006"><Response><Error><ErrorCode>{code}</ErrorCode><Message>{message}</Message></Error></Response></Autodiscover>"#
        );
    }
    let email = quick_xml::escape::escape(request.as_ref().unwrap().request.email.trim());
    let url = quick_xml::escape::escape(url);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006">
  <Response xmlns="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006">
    <Culture>en:us</Culture>
    <User><EMailAddress>{email}</EMailAddress></User>
    <Action><Settings><Server><Type>MobileSync</Type><Url>{url}</Url><Name>{url}</Name></Server></Settings></Action>
  </Response>
</Autodiscover>
"#
    )
}

pub async fn handler(State(state): State<AppState>, body: Bytes) -> impl IntoResponse {
    (
        StatusCode::OK,
        [("Content-Type", "text/xml; charset=utf-8")],
        response(&body, state.config.eas_public_url.as_str()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(email: &str, schema: &str) -> String {
        format!(
            r#"<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/requestschema/2006"><Request><EMailAddress>{email}</EMailAddress><AcceptableResponseSchema>{schema}</AcceptableResponseSchema></Request></Autodiscover>"#
        )
    }

    #[test]
    fn returns_requested_address_and_escapes_xml() {
        let input = request("a&amp;b@example.test", MOBILE_SCHEMA);
        let result = response(input.as_bytes(), "https://example.test/eas?a=1&b=2");
        assert!(result.contains("<EMailAddress>a&amp;b@example.test</EMailAddress>"));
        assert!(result.contains("<Url>https://example.test/eas?a=1&amp;b=2</Url>"));
        assert!(result.contains("<Culture>en:us</Culture>"));
    }

    #[test]
    fn returns_protocol_errors_for_invalid_requests_and_other_schemas() {
        assert!(response(b"not xml", "https://example.test").contains("<ErrorCode>600</ErrorCode>"));
        let wrong_namespace = request("a@example.test", MOBILE_SCHEMA).replace(
            "mobilesync/requestschema/2006",
            "outlook/requestschema/2006",
        );
        assert!(response(wrong_namespace.as_bytes(), "https://example.test")
            .contains("<ErrorCode>600</ErrorCode>"));
        let dtd = format!(
            "<!DOCTYPE Autodiscover [<!ENTITY x SYSTEM 'file:///etc/passwd'>]>{}",
            request("&x;", MOBILE_SCHEMA)
        );
        assert!(
            response(dtd.as_bytes(), "https://example.test").contains("<ErrorCode>600</ErrorCode>")
        );
        let wrong_root =
            request("a@example.test", MOBILE_SCHEMA).replace("Autodiscover", "WrongRoot");
        assert!(response(wrong_root.as_bytes(), "https://example.test")
            .contains("<ErrorCode>600</ErrorCode>"));
        let prefixed = format!(
            r#"<a:Autodiscover xmlns:a="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/requestschema/2006"><a:Request><a:EMailAddress>a@example.test</a:EMailAddress><a:AcceptableResponseSchema>{MOBILE_SCHEMA}</a:AcceptableResponseSchema></a:Request></a:Autodiscover>"#
        );
        assert!(response(prefixed.as_bytes(), "https://example.test")
            .contains("<EMailAddress>a@example.test</EMailAddress>"));
        assert!(response(
            request("a@example.test", "https://example.test/other").as_bytes(),
            "https://example.test"
        )
        .contains("<ErrorCode>601</ErrorCode>"));
        let duplicate = request("a@example.test", MOBILE_SCHEMA).replace(
            "</Autodiscover>",
            "<Request><EMailAddress>b@example.test</EMailAddress></Request></Autodiscover>",
        );
        assert!(response(duplicate.as_bytes(), "https://example.test")
            .contains("<ErrorCode>600</ErrorCode>"));
    }
}
