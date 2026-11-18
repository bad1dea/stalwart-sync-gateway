use anyhow::{bail, Context};
use base64::{engine::general_purpose::STANDARD, Engine};

pub(super) fn attach_original(mime: &[u8], original: &[u8]) -> anyhow::Result<Vec<u8>> {
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
    let body = &mime[header_end + separator..];
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
    fn rejects_malformed_or_already_signed_messages() {
        assert!(attach_original(b"bad header\r\n\r\nbody", b"x").is_err());
        assert!(attach_original(b"DKIM-Signature: abc\r\n\r\nbody", b"x").is_err());
        assert!(attach_original(b"Subject: test\r\nno separator", b"x").is_err());
    }
}
