use bytes::Bytes;
use thiserror::Error;

use super::token::{Token, END, OPAQUE, STR_I, SWITCH_PAGE};

const WBXML_VERSION_13: u8 = 0x03;
const PUBLIC_ID_UNKNOWN_OR_AS: u64 = 0x01;
const CHARSET_UTF8: u64 = 106;
const MAX_NESTING: usize = 64;
const MAX_TOKENS: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Start(Token),
    End,
    Text(String),
    Opaque(Bytes),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("input is empty")]
    Empty,
    #[error("unsupported WBXML version {0:#x}")]
    UnsupportedVersion(u8),
    #[error("unsupported public id {0}")]
    UnsupportedPublicId(u64),
    #[error("unsupported charset {0}")]
    UnsupportedCharset(u64),
    #[error("string table is not supported yet")]
    StringTableUnsupported,
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("invalid UTF-8 string")]
    InvalidUtf8,
    #[error("attributes are not supported by ActiveSync WBXML")]
    AttributesUnsupported,
    #[error("document nesting exceeded limit")]
    NestingLimit,
    #[error("token count exceeded limit")]
    TokenLimit,
}

pub fn decode_document(input: &[u8]) -> Result<Document, DecodeError> {
    let mut cursor = Cursor::new(input);
    let version = cursor.byte().ok_or(DecodeError::Empty)?;
    if version != WBXML_VERSION_13 {
        return Err(DecodeError::UnsupportedVersion(version));
    }
    let public_id = cursor.mb_uint()?;
    if public_id != PUBLIC_ID_UNKNOWN_OR_AS {
        return Err(DecodeError::UnsupportedPublicId(public_id));
    }
    let charset = cursor.mb_uint()?;
    if charset != CHARSET_UTF8 {
        return Err(DecodeError::UnsupportedCharset(charset));
    }
    let string_table_len = cursor.mb_uint()? as usize;
    if string_table_len != 0 {
        return Err(DecodeError::StringTableUnsupported);
    }

    let mut code_page = 0;
    let mut depth = 0usize;
    let mut count = 0usize;
    let mut nodes = Vec::new();

    while let Some(byte) = cursor.byte() {
        count += 1;
        if count > MAX_TOKENS {
            return Err(DecodeError::TokenLimit);
        }
        match byte {
            SWITCH_PAGE => {
                code_page = cursor.byte().ok_or(DecodeError::UnexpectedEof)?;
            }
            END => {
                depth = depth.saturating_sub(1);
                nodes.push(Node::End);
            }
            STR_I => nodes.push(Node::Text(cursor.inline_string()?)),
            OPAQUE => {
                let len = cursor.mb_uint()? as usize;
                nodes.push(Node::Opaque(cursor.bytes(len)?));
            }
            other => {
                let token = Token::from_byte(code_page, other);
                if token.has_attributes {
                    return Err(DecodeError::AttributesUnsupported);
                }
                if token.has_content {
                    depth += 1;
                    if depth > MAX_NESTING {
                        return Err(DecodeError::NestingLimit);
                    }
                }
                nodes.push(Node::Start(token));
            }
        }
    }

    Ok(Document { nodes })
}

struct Cursor<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    fn byte(&mut self) -> Option<u8> {
        let byte = *self.input.get(self.pos)?;
        self.pos += 1;
        Some(byte)
    }

    fn bytes(&mut self, len: usize) -> Result<Bytes, DecodeError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(DecodeError::UnexpectedEof)?;
        let bytes = self
            .input
            .get(self.pos..end)
            .ok_or(DecodeError::UnexpectedEof)?;
        self.pos = end;
        Ok(Bytes::copy_from_slice(bytes))
    }

    fn mb_uint(&mut self) -> Result<u64, DecodeError> {
        let mut value = 0u64;
        loop {
            let byte = self.byte().ok_or(DecodeError::UnexpectedEof)?;
            value = (value << 7) | u64::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
    }

    fn inline_string(&mut self) -> Result<String, DecodeError> {
        let start = self.pos;
        while let Some(byte) = self.byte() {
            if byte == 0 {
                let bytes = &self.input[start..self.pos - 1];
                return String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidUtf8);
            }
        }
        Err(DecodeError::UnexpectedEof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_minimal_sync_tree() {
        let input = [
            0x03, 0x01, 106, 0x00, 0x45, 0x4b, 0x03, b'0', 0x00, 0x01, 0x01,
        ];
        let doc = decode_document(&input).unwrap();
        assert_eq!(doc.nodes.len(), 5);
    }

    #[test]
    fn rejects_wrong_charset() {
        let err = decode_document(&[0x03, 0x01, 0x04, 0x00]).unwrap_err();
        assert_eq!(err, DecodeError::UnsupportedCharset(4));
    }

    #[test]
    fn rejects_deep_nesting() {
        let mut input = vec![0x03, 0x01, 106, 0x00];
        input.extend(std::iter::repeat_n(0x45, 65));
        let err = decode_document(&input).unwrap_err();
        assert_eq!(err, DecodeError::NestingLimit);
    }
}
