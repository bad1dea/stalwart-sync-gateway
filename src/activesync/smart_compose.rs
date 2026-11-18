use anyhow::{bail, Context};
use base64::{engine::general_purpose::STANDARD, Engine};

fn headers_and_body(mime: &[u8]) -> anyhow::Result<(Vec<Vec<u8>>, &[u8])> {
    let (header_end, separator) = [
        mime.windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .map(|pos| (pos, 4)),
        mime.windows(2)
            .position(|bytes| bytes == b"\n\n")
            .map(|pos| (pos, 2)),
    ]
    .into_iter()
    .flatten()
    .min_by_key(|(pos, _)| *pos)
    .context("message has no header separator")?;
    let mut headers: Vec<Vec<u8>> = Vec::new();
    for line in mime[..header_end].split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line
            .iter()
            .any(|byte| (*byte < 32 && *byte != b'\t') || *byte == 127)
        {
            bail!("invalid message header");
        }
        if line.starts_with(b" ") || line.starts_with(b"\t") {
            let previous = headers.last_mut().context("invalid header continuation")?;
            previous.extend_from_slice(b"\r\n");
            previous.extend_from_slice(line);
        } else {
            let name = line.split(|byte| *byte == b':').next().unwrap_or_default();
            if name.is_empty()
                || name.len() == line.len()
                || !name
                    .iter()
                    .all(|byte| (33..=126).contains(byte) && *byte != b':')
            {
                bail!("invalid message header");
            }
            headers.push(line.to_vec());
        }
    }
    Ok((headers, &mime[header_end + separator..]))
}

pub(super) fn attach_original(mime: &[u8], original: &[u8]) -> anyhow::Result<Vec<u8>> {
    let (headers, body) = headers_and_body(mime)?;
    let mut outer = Vec::new();
    let mut inner = Vec::new();
    for header in headers {
        let name = header
            .split(|byte| *byte == b':')
            .next()
            .unwrap_or_default();
        if name.eq_ignore_ascii_case(b"dkim-signature")
            || name.eq_ignore_ascii_case(b"domainkey-signature")
            || (name.len() >= 4 && name[..4].eq_ignore_ascii_case(b"arc-"))
        {
            bail!("cannot modify an already signed message");
        }
        if name.eq_ignore_ascii_case(b"mime-version") {
            continue;
        }
        let destination = if name.len() >= 8 && name[..8].eq_ignore_ascii_case(b"content-") {
            &mut inner
        } else {
            &mut outer
        };
        destination.extend_from_slice(&header);
        destination.extend_from_slice(b"\r\n");
    }
    let boundary = loop {
        let candidate = format!("eas-forward-{}", uuid::Uuid::new_v4().simple());
        if !body
            .windows(candidate.len())
            .any(|part| part == candidate.as_bytes())
        {
            break candidate;
        }
    };
    outer.extend_from_slice(format!("MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\n").as_bytes());
    outer.extend_from_slice(&inner);
    outer.extend_from_slice(b"\r\n");
    outer.extend_from_slice(body);
    outer.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Type: message/rfc822; name=\"forwarded_message.eml\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"forwarded_message.eml\"\r\n\r\n").as_bytes());
    let encoded = STANDARD.encode(original);
    for line in encoded.as_bytes().chunks(76) {
        outer.extend_from_slice(line);
        outer.extend_from_slice(b"\r\n");
    }
    outer.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(outer)
}

pub(super) fn inline_plain_reply(mime: &[u8], original: &str) -> anyhow::Result<Vec<u8>> {
    let (headers, body) = headers_and_body(mime)?;
    let mut output = Vec::new();
    let mut content_type = None;
    let mut encoding = None;
    for header in headers {
        let colon = header
            .iter()
            .position(|byte| *byte == b':')
            .context("invalid header")?;
        let name = &header[..colon];
        let value = std::str::from_utf8(&header[colon + 1..])?.replace("\r\n", " ");
        if name.eq_ignore_ascii_case(b"dkim-signature")
            || name.eq_ignore_ascii_case(b"domainkey-signature")
            || (name.len() >= 4 && name[..4].eq_ignore_ascii_case(b"arc-"))
        {
            bail!("cannot modify an already signed message");
        } else if name.eq_ignore_ascii_case(b"content-type") {
            if content_type.replace(value).is_some() {
                bail!("duplicate content type");
            }
        } else if name.eq_ignore_ascii_case(b"content-transfer-encoding") {
            if encoding
                .replace(value.trim().to_ascii_lowercase())
                .is_some()
            {
                bail!("duplicate transfer encoding");
            }
        } else if name.eq_ignore_ascii_case(b"mime-version") {
            continue;
        } else if name.len() >= 8 && name[..8].eq_ignore_ascii_case(b"content-") {
            bail!("unsupported message content header");
        } else {
            output.extend_from_slice(&header);
            output.extend_from_slice(b"\r\n");
        }
    }
    if let Some(content_type) = content_type {
        let mut parts = content_type.split(';');
        if !parts
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("text/plain")
        {
            bail!("reply is not plain text");
        }
        for parameter in parts {
            let (name, value) = parameter
                .trim()
                .split_once('=')
                .context("invalid content type")?;
            if !name.trim().eq_ignore_ascii_case("charset")
                || !matches!(
                    value.trim().trim_matches('"').to_ascii_lowercase().as_str(),
                    "utf-8" | "us-ascii"
                )
            {
                bail!("unsupported reply charset or content parameter");
            }
        }
    }
    let decoded = match encoding.as_deref().unwrap_or("7bit") {
        "7bit" | "8bit" => body.to_vec(),
        "base64" => STANDARD.decode(
            body.iter()
                .copied()
                .filter(|byte| !byte.is_ascii_whitespace())
                .collect::<Vec<_>>(),
        )?,
        "quoted-printable" => decode_quoted_printable(body)?,
        _ => bail!("unsupported reply transfer encoding"),
    };
    let text = std::str::from_utf8(&decoded)?;
    anyhow::ensure!(
        !text.contains('\0') && !original.contains('\0'),
        "invalid reply text"
    );
    let mut result = text.to_owned();
    if !result.ends_with('\n') {
        result.push_str("\r\n");
    }
    result.push_str("\r\n");
    for line in original.lines() {
        result.push_str("> ");
        result.push_str(line.trim_end_matches('\r'));
        result.push_str("\r\n");
    }
    output.extend_from_slice(b"MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n");
    let encoded = STANDARD.encode(result.as_bytes());
    for line in encoded.as_bytes().chunks(76) {
        output.extend_from_slice(line);
        output.extend_from_slice(b"\r\n");
    }
    Ok(output)
}

fn decode_quoted_printable(body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut decoded = Vec::new();
    let mut index = 0;
    while index < body.len() {
        if body[index] != b'=' {
            decoded.push(body[index]);
            index += 1;
            continue;
        }
        if body.get(index + 1..index + 3) == Some(b"\r\n") {
            index += 3;
            continue;
        }
        if body.get(index + 1) == Some(&b'\n') {
            index += 2;
            continue;
        }
        let hex = body
            .get(index + 1..index + 3)
            .context("invalid quoted-printable")?;
        anyhow::ensure!(
            hex.iter().all(u8::is_ascii_hexdigit),
            "invalid quoted-printable"
        );
        let hex = std::str::from_utf8(hex)?;
        decoded.push(
            u8::from_str_radix(hex, 16).map_err(|_| anyhow::anyhow!("invalid quoted-printable"))?,
        );
        index += 3;
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_headers_body_and_binary_original() {
        let mime = b"From: a@example.test\r\nTo: b@example.test\r\nSubject: folded\r\n continuation\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\nForward text";
        let original = b"From: c@example.test\r\n\r\nOriginal\0body";
        let output = attach_original(mime, original).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Subject: folded\r\n continuation\r\n"));
        assert!(text.contains("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\nForward text"));
        assert!(text.contains(&STANDARD.encode(original)));
        assert!(text.contains("Content-Type: message/rfc822"));
    }
    #[test]
    fn preserves_existing_multipart_content_with_lf_headers() {
        let mime = b"From: a@example.test\nContent-Type: multipart/mixed; boundary=inner\n\n--inner\r\nContent-Type: text/plain\r\n\r\nhello\r\n--inner--\r\n";
        let output = attach_original(mime, b"original").unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Content-Type: multipart/mixed; boundary=inner\r\n\r\n--inner\r\nContent-Type: text/plain\r\n\r\nhello\r\n--inner--"));
    }
    #[test]
    fn quotes_plain_reply_and_decodes_transfer_encodings() {
        for (encoding, body) in [
            ("8bit", "Hello"),
            ("base64", "SGVsbG8="),
            ("quoted-printable", "Hel=6Co"),
        ] {
            let mime = format!("From: a@example.test\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: {encoding}\r\n\r\n{body}");
            let reply = inline_plain_reply(mime.as_bytes(), "Original line\nNext line").unwrap();
            let body = std::str::from_utf8(&reply)
                .unwrap()
                .split("\r\n\r\n")
                .nth(1)
                .unwrap()
                .replace("\r\n", "");
            assert_eq!(
                String::from_utf8(STANDARD.decode(body).unwrap()).unwrap(),
                "Hello\r\n\r\n> Original line\r\n> Next line\r\n"
            );
        }
        assert!(inline_plain_reply(b"Content-Type: text/html\r\n\r\nhello", "original").is_err());
        assert!(inline_plain_reply(
            b"Content-Type: text/plain; charset=iso-8859-1\r\n\r\nhello",
            "original"
        )
        .is_err());
        assert!(inline_plain_reply(
            b"Content-Transfer-Encoding: quoted-printable\r\n\r\ninvalid=QZ",
            "original"
        )
        .is_err());
    }
    #[test]
    fn rejects_malformed_or_already_signed_messages() {
        assert!(attach_original(b"bad header\r\n\r\nbody", b"x").is_err());
        assert!(attach_original(b"DKIM-Signature: abc\r\n\r\nbody", b"x").is_err());
        assert!(attach_original(b"Subject: test\r\nno separator", b"x").is_err());
    }
}
