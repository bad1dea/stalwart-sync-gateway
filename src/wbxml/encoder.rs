use super::{token, Document, Node};

pub fn encode_document(document: &Document) -> Vec<u8> {
    let mut out = vec![0x03, 0x01, 106, 0x00];
    let mut code_page = 0u8;
    for node in &document.nodes {
        match node {
            Node::Start(t) => {
                if t.code_page != code_page {
                    out.push(token::SWITCH_PAGE);
                    out.push(t.code_page);
                    code_page = t.code_page;
                }
                out.push(t.to_byte());
            }
            Node::End => out.push(token::END),
            Node::Text(text) => {
                out.push(token::STR_I);
                out.extend_from_slice(text.replace('\0', "").as_bytes());
                out.push(0);
            }
            Node::Opaque(bytes) => {
                out.push(token::OPAQUE);
                write_mb_uint(bytes.len() as u64, &mut out);
                out.extend_from_slice(bytes);
            }
        }
    }
    out
}

fn write_mb_uint(mut value: u64, out: &mut Vec<u8>) {
    let mut bytes = [0u8; 10];
    let mut i = bytes.len();
    bytes[i - 1] = (value & 0x7f) as u8;
    i -= 1;
    value >>= 7;
    while value > 0 {
        bytes[i - 1] = ((value & 0x7f) as u8) | 0x80;
        i -= 1;
        value >>= 7;
    }
    out.extend_from_slice(&bytes[i..]);
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use crate::wbxml::{decode_document, token::Token, Document, Node};

    use super::encode_document;

    #[test]
    fn round_trips_document() {
        let doc = Document {
            nodes: vec![
                Node::Start(Token {
                    code_page: 0,
                    token: 0x05,
                    has_content: true,
                    has_attributes: false,
                }),
                Node::Text("hello".to_owned()),
                Node::Opaque(Bytes::from_static(b"abc")),
                Node::End,
            ],
        };
        let encoded = encode_document(&doc);
        assert_eq!(decode_document(&encoded).unwrap(), doc);
    }
}
