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
    pub const CHANGE: Token = tag(0x08, false);
    pub const DELETE: Token = tag(0x09, false);
    pub const FETCH: Token = tag(0x0a, false);
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

pub mod move_items {
    use super::Token;

    pub const PAGE: u8 = 5;
    pub const MOVES: Token = tag(0x05, false);
    pub const MOVE: Token = tag(0x06, false);
    pub const SRC_MSG_ID: Token = tag(0x07, false);
    pub const SRC_FLD_ID: Token = tag(0x08, false);
    pub const DST_FLD_ID: Token = tag(0x09, false);
    pub const RESPONSE: Token = tag(0x0a, false);
    pub const STATUS: Token = tag(0x0b, false);
    pub const DST_MSG_ID: Token = tag(0x0c, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

pub mod get_item_estimate {
    use super::Token;

    pub const PAGE: u8 = 6;
    pub const GET_ITEM_ESTIMATE: Token = tag(0x05, false);
    pub const FOLDERS: Token = tag(0x07, false);
    pub const FOLDER: Token = tag(0x08, false);
    pub const FOLDER_TYPE: Token = tag(0x09, false);
    pub const FOLDER_ID: Token = tag(0x0a, false);
    pub const ESTIMATE: Token = tag(0x0c, false);
    pub const RESPONSE: Token = tag(0x0d, false);
    pub const STATUS: Token = tag(0x0e, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

pub mod provision {
    use super::Token;

    pub const PAGE: u8 = 14;
    pub const PROVISION: Token = tag(0x05, false);
    pub const POLICIES: Token = tag(0x06, false);
    pub const POLICY: Token = tag(0x07, false);
    pub const POLICY_TYPE: Token = tag(0x08, false);
    pub const POLICY_KEY: Token = tag(0x09, false);
    pub const STATUS: Token = tag(0x0b, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

pub mod settings {
    use super::Token;

    pub const PAGE: u8 = 18;
    pub const SETTINGS: Token = tag(0x05, false);
    pub const STATUS: Token = tag(0x06, false);
    pub const GET: Token = tag(0x07, false);
    pub const USER_INFORMATION: Token = tag(0x1d, false);
    pub const EMAIL_ADDRESSES: Token = tag(0x1e, false);
    pub const SMTP_ADDRESS: Token = tag(0x1f, false);
    pub const PRIMARY_SMTP_ADDRESS: Token = tag(0x23, false);

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
    pub commands: Vec<SyncClientCommand>,
}

impl Default for SyncCollectionRequest {
    fn default() -> Self {
        Self {
            sync_key: "0".to_owned(),
            collection_id: String::new(),
            window_size: 25,
            get_changes: true,
            commands: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncClientCommand {
    pub kind: SyncClientCommandKind,
    pub server_id: String,
    pub read: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncClientCommandKind {
    Add,
    Change,
    Delete,
    Fetch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveItemRequest {
    pub src_msg_id: String,
    pub src_fld_id: String,
    pub dst_fld_id: String,
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

pub fn find_all_text_after(document: &Document, token: Token) -> Vec<String> {
    document
        .nodes
        .windows(2)
        .filter_map(|pair| match pair {
            [Node::Start(start), Node::Text(text)]
                if start.code_page == token.code_page && start.token == token.token =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

pub fn contains_token(document: &Document, token: Token) -> bool {
    document.nodes.iter().any(|node| match node {
        Node::Start(start) => start.code_page == token.code_page && start.token == token.token,
        Node::Text(_) | Node::End | Node::Opaque(_) => false,
    })
}

pub fn sync_collections(document: &Document) -> Vec<SyncCollectionRequest> {
    let mut collections = Vec::new();
    let mut current: Option<SyncCollectionRequest> = None;
    let mut current_command: Option<SyncClientCommand> = None;
    let mut commands_depth: Option<usize> = None;
    let mut pending_leaf: Option<Token> = None;
    let mut stack: Vec<Token> = Vec::new();

    for node in &document.nodes {
        match node {
            Node::Start(token)
                if token.code_page == airsync::PAGE && token.token == airsync::COLLECTION.token =>
            {
                current = Some(SyncCollectionRequest::default());
                if token.has_content {
                    stack.push(*token);
                }
            }
            Node::Start(token) => {
                if current.is_some() {
                    if token.code_page == airsync::PAGE && token.token == airsync::COMMANDS.token {
                        commands_depth = Some(stack.len());
                    } else if commands_depth.is_some()
                        && current_command.is_none()
                        && stack
                            .last()
                            .is_some_and(|parent| same_token(*parent, airsync::COMMANDS))
                    {
                        current_command = command_kind(*token).map(|kind| SyncClientCommand {
                            kind,
                            server_id: String::new(),
                            read: None,
                        });
                    } else {
                        pending_leaf = Some(*token);
                    }
                }
                if token.has_content {
                    stack.push(*token);
                }
            }
            Node::Text(text) => {
                if let Some(command) = current_command.as_mut() {
                    if let Some(token) = pending_leaf.take() {
                        if same_token(token, airsync::SERVER_ID) {
                            command.server_id = text.clone();
                        } else if same_token(token, email::READ) {
                            command.read = Some(text == "1");
                        }
                    }
                } else if let Some(collection) = current.as_mut() {
                    if let Some(token) = pending_leaf.take() {
                        if token.code_page == airsync::PAGE
                            && token.token == airsync::SYNC_KEY.token
                        {
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
            }
            Node::End => {
                let ended = stack.pop();
                if let Some(ended) = ended {
                    if same_token(ended, airsync::COMMANDS) {
                        commands_depth = None;
                    }
                }
                if ended.and_then(command_kind).is_some() {
                    if let (Some(collection), Some(command)) =
                        (current.as_mut(), current_command.take())
                    {
                        if !command.server_id.is_empty() {
                            collection.commands.push(command);
                        }
                    }
                }
                if ended.is_some_and(|token| same_token(token, airsync::COLLECTION)) {
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

pub fn move_item_requests(document: &Document) -> Vec<MoveItemRequest> {
    let mut moves = Vec::new();
    let mut current: Option<MoveItemRequest> = None;
    let mut pending_leaf: Option<Token> = None;
    let mut stack: Vec<Token> = Vec::new();

    for node in &document.nodes {
        match node {
            Node::Start(token) if same_token(*token, move_items::MOVE) => {
                current = Some(MoveItemRequest {
                    src_msg_id: String::new(),
                    src_fld_id: String::new(),
                    dst_fld_id: String::new(),
                });
                if token.has_content {
                    stack.push(*token);
                }
            }
            Node::Start(token) => {
                if current.is_some() {
                    pending_leaf = Some(*token);
                }
                if token.has_content {
                    stack.push(*token);
                }
            }
            Node::Text(text) => {
                if let (Some(current), Some(token)) = (current.as_mut(), pending_leaf.take()) {
                    if same_token(token, move_items::SRC_MSG_ID) {
                        current.src_msg_id = text.clone();
                    } else if same_token(token, move_items::SRC_FLD_ID) {
                        current.src_fld_id = text.clone();
                    } else if same_token(token, move_items::DST_FLD_ID) {
                        current.dst_fld_id = text.clone();
                    }
                }
            }
            Node::End => {
                let ended = stack.pop();
                if ended.is_some_and(|token| same_token(token, move_items::MOVE)) {
                    if let Some(move_request) = current.take() {
                        if !move_request.src_msg_id.is_empty()
                            && !move_request.src_fld_id.is_empty()
                            && !move_request.dst_fld_id.is_empty()
                        {
                            moves.push(move_request);
                        }
                    }
                }
                pending_leaf = None;
            }
            Node::Opaque(_) => {}
        }
    }

    moves
}

fn same_token(left: Token, right: Token) -> bool {
    left.code_page == right.code_page && left.token == right.token
}

fn command_kind(token: Token) -> Option<SyncClientCommandKind> {
    if same_token(token, airsync::ADD) {
        Some(SyncClientCommandKind::Add)
    } else if same_token(token, airsync::CHANGE) {
        Some(SyncClientCommandKind::Change)
    } else if same_token(token, airsync::DELETE) {
        Some(SyncClientCommandKind::Delete)
    } else if same_token(token, airsync::FETCH) {
        Some(SyncClientCommandKind::Fetch)
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sync_client_change_and_delete_commands() {
        let mut builder = DocumentBuilder::new();
        builder.start(airsync::SYNC);
        builder.start(airsync::COLLECTIONS);
        builder.start(airsync::COLLECTION);
        builder.leaf(airsync::SYNC_KEY, "1");
        builder.leaf(airsync::COLLECTION_ID, "inbox");
        builder.start(airsync::COMMANDS);
        builder.start(airsync::CHANGE);
        builder.leaf(airsync::SERVER_ID, "email-a");
        builder.start(airsync::APPLICATION_DATA);
        builder.leaf(email::READ, "1");
        builder.end();
        builder.end();
        builder.start(airsync::DELETE);
        builder.leaf(airsync::SERVER_ID, "email-b");
        builder.end();
        builder.end();
        builder.end();
        builder.end();
        builder.end();

        let collections = sync_collections(&builder.finish());

        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].collection_id, "inbox");
        assert_eq!(collections[0].commands.len(), 2);
        assert_eq!(
            collections[0].commands[0].kind,
            SyncClientCommandKind::Change
        );
        assert_eq!(collections[0].commands[0].server_id, "email-a");
        assert_eq!(collections[0].commands[0].read, Some(true));
        assert_eq!(
            collections[0].commands[1].kind,
            SyncClientCommandKind::Delete
        );
        assert_eq!(collections[0].commands[1].server_id, "email-b");
    }

    #[test]
    fn parses_move_items_request() {
        let mut builder = DocumentBuilder::new();
        builder.start(move_items::MOVES);
        builder.start(move_items::MOVE);
        builder.leaf(move_items::SRC_MSG_ID, "email-a");
        builder.leaf(move_items::SRC_FLD_ID, "inbox");
        builder.leaf(move_items::DST_FLD_ID, "archive");
        builder.end();
        builder.end();

        let moves = move_item_requests(&builder.finish());

        assert_eq!(
            moves,
            vec![MoveItemRequest {
                src_msg_id: "email-a".to_owned(),
                src_fld_id: "inbox".to_owned(),
                dst_fld_id: "archive".to_owned(),
            }]
        );
    }
}
