use crate::wbxml::{token::Token, Document, Node};

pub mod folder_hierarchy {
    use super::Token;

    pub const PAGE: u8 = 7;
    pub const DISPLAY_NAME: Token = tag(0x07, false);
    pub const SERVER_ID: Token = tag(0x08, false);
    pub const PARENT_ID: Token = tag(0x09, false);
    pub const TYPE: Token = tag(0x0a, false);
    pub const STATUS: Token = tag(0x0c, false);
    pub const CHANGES: Token = tag(0x0e, false);
    pub const ADD: Token = tag(0x0f, false);
    pub const SYNC_KEY: Token = tag(0x12, false);
    pub const FOLDER_SYNC: Token = tag(0x16, false);
    pub const COUNT: Token = tag(0x17, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

pub mod airsync {
    use super::Token;

    pub const PAGE: u8 = 0;
    pub const SYNC: Token = tag(0x05, false);
    pub const ADD: Token = tag(0x07, false);
    pub const SYNC_KEY: Token = tag(0x0b, false);
    pub const SERVER_ID: Token = tag(0x0d, false);
    pub const STATUS: Token = tag(0x0e, false);
    pub const COLLECTION: Token = tag(0x0f, false);
    pub const COLLECTION_ID: Token = tag(0x12, false);
    pub const GET_CHANGES: Token = tag(0x13, false);
    pub const WINDOW_SIZE: Token = tag(0x15, false);
    pub const COMMANDS: Token = tag(0x16, false);
    pub const COLLECTIONS: Token = tag(0x1c, false);
    pub const APPLICATION_DATA: Token = tag(0x1d, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

pub mod email {
    use super::Token;

    pub const PAGE: u8 = 2;
    pub const DATE_RECEIVED: Token = tag(0x0f, false);
    pub const DISPLAY_TO: Token = tag(0x11, false);
    pub const IMPORTANCE: Token = tag(0x12, false);
    pub const MESSAGE_CLASS: Token = tag(0x13, false);
    pub const SUBJECT: Token = tag(0x14, false);
    pub const READ: Token = tag(0x15, false);
    pub const TO: Token = tag(0x16, false);
    pub const CC: Token = tag(0x17, false);
    pub const FROM: Token = tag(0x18, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

pub mod airsync_base {
    use super::Token;

    pub const PAGE: u8 = 17;
    pub const TYPE: Token = tag(0x06, false);
    pub const BODY: Token = tag(0x0a, false);
    pub const DATA: Token = tag(0x0b, false);
    pub const ESTIMATED_DATA_SIZE: Token = tag(0x0c, false);
    pub const TRUNCATED: Token = tag(0x0d, false);
    pub const NATIVE_BODY_TYPE: Token = tag(0x15, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncCollectionRequest {
    pub sync_key: String,
    pub collection_id: String,
    pub window_size: usize,
    pub get_changes: bool,
}

impl Default for SyncCollectionRequest {
    fn default() -> Self {
        Self {
            sync_key: "0".to_owned(),
            collection_id: String::new(),
            window_size: 25,
            get_changes: true,
        }
    }
}

pub fn find_text_after(document: &Document, token: Token) -> Option<&str> {
    document.nodes.windows(2).find_map(|pair| match pair {
        [Node::Start(start), Node::Text(text)]
            if start.code_page == token.code_page && start.token == token.token =>
        {
            Some(text.as_str())
        }
        _ => None,
    })
}

pub fn sync_collections(document: &Document) -> Vec<SyncCollectionRequest> {
    let mut collections = Vec::new();
    let mut current: Option<SyncCollectionRequest> = None;
    let mut current_depth = 0usize;
    let mut pending_leaf: Option<Token> = None;
    let mut depth = 0usize;

    for node in &document.nodes {
        match node {
            Node::Start(token)
                if token.code_page == airsync::PAGE && token.token == airsync::COLLECTION.token =>
            {
                current = Some(SyncCollectionRequest::default());
                current_depth = depth;
                if token.has_content {
                    depth += 1;
                }
            }
            Node::Start(token) => {
                if current.is_some() {
                    pending_leaf = Some(*token);
                }
                if token.has_content {
                    depth += 1;
                }
            }
            Node::Text(text) => {
                if let (Some(collection), Some(token)) = (current.as_mut(), pending_leaf.take()) {
                    if token.code_page == airsync::PAGE && token.token == airsync::SYNC_KEY.token {
                        collection.sync_key = text.clone();
                    } else if token.code_page == airsync::PAGE
                        && token.token == airsync::COLLECTION_ID.token
                    {
                        collection.collection_id = text.clone();
                    } else if token.code_page == airsync::PAGE
                        && token.token == airsync::WINDOW_SIZE.token
                    {
                        collection.window_size = text.parse().unwrap_or(25);
                    } else if token.code_page == airsync::PAGE
                        && token.token == airsync::GET_CHANGES.token
                    {
                        collection.get_changes = text != "0";
                    }
                }
            }
            Node::End => {
                depth = depth.saturating_sub(1);
                if current.is_some() && depth == current_depth {
                    if let Some(collection) = current.take() {
                        if !collection.collection_id.is_empty() {
                            collections.push(collection);
                        }
                    }
                }
                pending_leaf = None;
            }
            Node::Opaque(_) => {}
        }
    }

    collections
}

pub struct DocumentBuilder {
    nodes: Vec<Node>,
}

impl DocumentBuilder {
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    pub fn start(&mut self, token: Token) {
        let mut token = token;
        token.has_content = true;
        self.nodes.push(Node::Start(token));
    }

    pub fn leaf(&mut self, token: Token, text: impl Into<String>) {
        let mut token = token;
        token.has_content = true;
        self.nodes.push(Node::Start(token));
        self.nodes.push(Node::Text(text.into()));
        self.nodes.push(Node::End);
    }

    pub fn end(&mut self) {
        self.nodes.push(Node::End);
    }

    pub fn finish(self) -> Document {
        Document { nodes: self.nodes }
    }
}

impl Default for DocumentBuilder {
    fn default() -> Self {
        Self::new()
    }
}
