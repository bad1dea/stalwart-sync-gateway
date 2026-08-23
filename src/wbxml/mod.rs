mod decoder;
pub mod eas;
mod encoder;
pub mod token;

pub use decoder::{decode_document, Document, Node};
pub use encoder::encode_document;
