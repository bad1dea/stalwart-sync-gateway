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
    // Per Z-Push's own authoritative WBXML DTD table (lib/wbxml/wbxmldefs.php,
    // codepage 0): 0x06 = "Replies" (aka Responses -- the section that
    // confirms client-submitted Add/Change/Delete back to the client, as
    // opposed to Commands, which pushes server-originated changes TO the
    // client). Verified against that source directly, not assumed.
    pub const RESPONSES: Token = tag(0x06, false);
    pub const ADD: Token = tag(0x07, false);
    pub const CHANGE: Token = tag(0x08, false);
    pub const DELETE: Token = tag(0x09, false);
    pub const FETCH: Token = tag(0x0a, false);
    pub const SYNC_KEY: Token = tag(0x0b, false);
    pub const CLIENT_ID: Token = tag(0x0c, false);
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

/// ActiveSync "Notes" class (MS-ASNOTE). Codepage 23 (0x17) -- verified
/// against Z-Push's own WBXML DTD table, not assumed: an earlier guess of
/// codepage 10 was wrong (that's POOMTASKS/ResolveRecipients). Field order
/// and tag numbers below match a live device trace captured against this
/// exact fork's PHP predecessor.
pub mod notes {
    use super::Token;

    pub const PAGE: u8 = 23;
    pub const SUBJECT: Token = tag(0x05, false);
    pub const MESSAGE_CLASS: Token = tag(0x06, false);
    pub const LAST_MODIFIED_DATE: Token = tag(0x07, false);
    pub const CATEGORIES: Token = tag(0x08, false);
    pub const CATEGORY: Token = tag(0x09, false);

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
    // Per MS-ASWBXML codepage 17: 0x15 is IsInline, NOT NativeBodyType --
    // this constant was wrong (0x15) for a long time, meaning every synced
    // message wrote a nonsensical value ("2", a body-type constant) into
    // the IsInline field, which is normally a 0/1 boolean used for inline-
    // attachment context, and NEVER sent the real NativeBodyType at all.
    pub const NATIVE_BODY_TYPE: Token = tag(0x16, false);
    pub const PREVIEW: Token = tag(0x18, false);

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

pub mod ping {
    use super::Token;

    pub const PAGE: u8 = 13;
    pub const PING: Token = tag(0x05, false);
    pub const STATUS: Token = tag(0x06, false);
    pub const LIFETIME: Token = tag(0x07, false);
    pub const FOLDERS: Token = tag(0x08, false);
    pub const FOLDER: Token = tag(0x09, false);
    pub const SERVER_ENTRY_ID: Token = tag(0x0a, false);
    pub const FOLDER_TYPE: Token = tag(0x0b, false);
    pub const MAX_FOLDERS: Token = tag(0x0c, false);

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
    pub const SET: Token = tag(0x08, false);
    pub const OOF: Token = tag(0x09, false);
    pub const OOF_STATE: Token = tag(0x0a, false);
    pub const START_TIME: Token = tag(0x0b, false);
    pub const END_TIME: Token = tag(0x0c, false);
    pub const OOF_MESSAGE: Token = tag(0x0d, false);
    pub const APPLIES_TO_INTERNAL: Token = tag(0x0e, false);
    pub const APPLIES_TO_EXTERNAL_KNOWN: Token = tag(0x0f, false);
    pub const APPLIES_TO_EXTERNAL_UNKNOWN: Token = tag(0x10, false);
    pub const ENABLED: Token = tag(0x11, false);
    pub const REPLY_MESSAGE: Token = tag(0x12, false);
    pub const BODY_TYPE: Token = tag(0x13, false);
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

/// MS-ASWBXML codepage 20 (ItemOperations). Numbering per spec, consistent
/// with the Get/Set/Status pattern already scaffolded in `settings`.
pub mod item_operations {
    use super::Token;

    pub const PAGE: u8 = 20;
    pub const ITEM_OPERATIONS: Token = tag(0x05, false);
    pub const FETCH: Token = tag(0x06, false);
    pub const STORE: Token = tag(0x07, false);
    pub const STATUS: Token = tag(0x0d, false);
    pub const RESPONSE: Token = tag(0x0e, false);
    pub const PROPERTIES: Token = tag(0x0b, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// MS-ASWBXML codepage 21 (ComposeMail) -- SendMail/SmartForward/
/// SmartReply. Numbering per spec, consistent with the pattern already
/// verified for Settings (18) and ItemOperations (20) in this file.
pub mod compose_mail {
    use super::Token;

    pub const PAGE: u8 = 21;
    pub const SEND_MAIL: Token = tag(0x05, false);
    pub const SMART_FORWARD: Token = tag(0x06, false);
    pub const SMART_REPLY: Token = tag(0x07, false);
    pub const SAVE_IN_SENT_ITEMS: Token = tag(0x08, false);
    pub const SOURCE: Token = tag(0x0b, false);
    pub const FOLDER_ID: Token = tag(0x0c, false);
    pub const ITEM_ID: Token = tag(0x0d, false);
    pub const MIME: Token = tag(0x10, false);
    pub const CLIENT_ID: Token = tag(0x11, false);
    pub const STATUS: Token = tag(0x12, false);

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
    /// Client-generated temp id, present on Add only (the item has no
    /// ServerId yet -- that's what the gateway is about to assign).
    pub client_id: String,
    /// The item's ServerId. Present on Change/Delete/Fetch; empty on Add.
    pub server_id: String,
    pub read: Option<bool>,
    pub note: NoteFields,
}

/// ActiveSync Notes class fields decoded from one Add/Change command's
/// ApplicationData (MS-ASNOTE + shared AirSyncBase:Body). `body_type`
/// mirrors AirSyncBase's Type element (1 = plain, 2 = html) verbatim --
/// callers map it to `EmailBodyType`/local Note types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoteFields {
    pub subject: Option<String>,
    pub body_type: Option<u8>,
    pub body: Option<String>,
    pub categories: Vec<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemOperationsFetchRequest {
    pub store: String,
    pub collection_id: String,
    pub server_id: String,
}

/// Parses an `ItemOperations > Fetch` request addressed the "Mailbox
/// Store" way (Store/CollectionId/ServerId) -- the form iOS uses to fetch
/// a message opened from a prior Sync result. Ignores `Options` (body
/// preference/truncation) -- the gateway always returns the full body
/// regardless, same as Sync does.
pub fn item_operations_fetch(document: &Document) -> Option<ItemOperationsFetchRequest> {
    let store = find_text_after(document, item_operations::STORE)?.to_owned();
    let collection_id = find_text_after(document, airsync::COLLECTION_ID)?.to_owned();
    let server_id = find_text_after(document, airsync::SERVER_ID)?.to_owned();
    Some(ItemOperationsFetchRequest {
        store,
        collection_id,
        server_id,
    })
}

/// Finds the raw bytes of an opaque (binary) node immediately following a
/// Start token -- used for `ComposeMail:Mime`, which carries the raw
/// RFC822 message as WBXML opaque data rather than a text string.
pub fn find_opaque_after(document: &Document, token: Token) -> Option<&[u8]> {
    document.nodes.windows(2).find_map(|pair| match pair {
        [Node::Start(start), Node::Opaque(bytes)]
            if start.code_page == token.code_page && start.token == token.token =>
        {
            Some(bytes.as_ref())
        }
        _ => None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeMailRequest {
    pub mime: Vec<u8>,
    pub save_in_sent_items: bool,
    pub source_item_id: Option<String>,
}

/// Parses a WBXML-wrapped SendMail/SmartForward/SmartReply request. Real
/// EAS 14.0+ clients more commonly send the MIME as the raw, un-wrapped
/// POST body instead (see the query-string form handled in
/// `activesync::send_mail`) -- this covers the WBXML-wrapped form for
/// completeness/older clients.
pub fn compose_mail_request(document: &Document) -> Option<ComposeMailRequest> {
    let mime = find_opaque_after(document, compose_mail::MIME)?.to_vec();
    let save_in_sent_items = contains_token(document, compose_mail::SAVE_IN_SENT_ITEMS);
    let source_item_id = find_text_after(document, compose_mail::ITEM_ID).map(str::to_owned);
    Some(ComposeMailRequest {
        mime,
        save_in_sent_items,
        source_item_id,
    })
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
    let mut current_command_start: Option<usize> = None;
    let mut commands_depth: Option<usize> = None;
    let mut pending_leaf: Option<Token> = None;
    let mut stack: Vec<Token> = Vec::new();

    for (idx, node) in document.nodes.iter().enumerate() {
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
                            client_id: String::new(),
                            server_id: String::new(),
                            read: None,
                            note: NoteFields::default(),
                        });
                        current_command_start = Some(idx);
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
                        } else if same_token(token, airsync::CLIENT_ID) {
                            command.client_id = text.clone();
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
                    if let (Some(collection), Some(mut command), Some(start)) = (
                        current.as_mut(),
                        current_command.take(),
                        current_command_start.take(),
                    ) {
                        // Everything strictly between the command's own
                        // Start and this End is its ApplicationData subtree
                        // -- extract Notes fields from it regardless of
                        // command kind (Delete/Fetch simply won't have any).
                        command.note = extract_note_fields(&document.nodes[start + 1..idx]);
                        let has_identity =
                            !command.server_id.is_empty() || !command.client_id.is_empty();
                        if has_identity {
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

/// Walks the nodes strictly inside one Add/Change command (i.e. inside its
/// `<ApplicationData>`) and pulls out the ActiveSync Notes fields, if any
/// are present. Tracks a full ancestor path (not just the immediate
/// pending tag) so nested elements -- `AirSyncBase:Body > Type`/`Data`,
/// `Notes:Categories > Notes:Category` (repeated) -- resolve unambiguously,
/// unlike the single-level `pending_leaf` lookahead used for flat Email
/// fields above.
fn extract_note_fields(nodes: &[Node]) -> NoteFields {
    let mut fields = NoteFields::default();
    let mut path: Vec<Token> = Vec::new();
    let mut category_buf: Option<String> = None;

    for node in nodes {
        match node {
            Node::Start(token) => path.push(*token),
            Node::Text(text) => {
                let Some(&top) = path.last() else { continue };
                let parent = path.len().checked_sub(2).map(|i| path[i]);
                if same_token(top, notes::SUBJECT) {
                    fields.subject = Some(text.clone());
                } else if same_token(top, airsync_base::TYPE)
                    && parent.is_some_and(|p| same_token(p, airsync_base::BODY))
                {
                    fields.body_type = text.parse::<u8>().ok();
                } else if same_token(top, airsync_base::DATA)
                    && parent.is_some_and(|p| same_token(p, airsync_base::BODY))
                {
                    fields.body = Some(text.clone());
                } else if same_token(top, notes::CATEGORY) {
                    category_buf = Some(text.clone());
                }
            }
            Node::End => {
                if let Some(top) = path.pop() {
                    if same_token(top, notes::CATEGORY) {
                        if let Some(category) = category_buf.take() {
                            fields.categories.push(category);
                        }
                    }
                }
            }
            Node::Opaque(_) => {}
        }
    }

    fields
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
    fn parses_note_add_with_client_id_body_and_categories() {
        // Mirrors a real device Add: ClientId (no ServerId yet -- the
        // whole point), nested AirSyncBase:Body (Type + Data), and
        // repeated Notes:Category inside Notes:Categories. This is the
        // structure the flat pending_leaf lookahead alone can't resolve;
        // extract_note_fields()'s ancestor-path walk is what this test is
        // actually exercising.
        let mut builder = DocumentBuilder::new();
        builder.start(airsync::SYNC);
        builder.start(airsync::COLLECTIONS);
        builder.start(airsync::COLLECTION);
        builder.leaf(airsync::SYNC_KEY, "1");
        builder.leaf(airsync::COLLECTION_ID, "note_x");
        builder.start(airsync::COMMANDS);
        builder.start(airsync::ADD);
        builder.leaf(airsync::CLIENT_ID, "tmp-1");
        builder.start(airsync::APPLICATION_DATA);
        builder.leaf(notes::SUBJECT, "Tag1");
        builder.start(airsync_base::BODY);
        builder.leaf(airsync_base::TYPE, "2");
        builder.leaf(airsync_base::DATA, "<html>hi</html>");
        builder.end();
        builder.leaf(notes::MESSAGE_CLASS, "IPM.StickyNote");
        builder.start(notes::CATEGORIES);
        builder.leaf(notes::CATEGORY, "Work");
        builder.leaf(notes::CATEGORY, "Personal");
        builder.end();
        builder.end();
        builder.end();
        builder.end();
        builder.end();
        builder.end();
        builder.end();

        let collections = sync_collections(&builder.finish());

        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].commands.len(), 1);
        let command = &collections[0].commands[0];
        assert_eq!(command.kind, SyncClientCommandKind::Add);
        assert_eq!(command.client_id, "tmp-1");
        assert_eq!(command.server_id, "");
        assert_eq!(command.note.subject.as_deref(), Some("Tag1"));
        assert_eq!(command.note.body_type, Some(2));
        assert_eq!(command.note.body.as_deref(), Some("<html>hi</html>"));
        assert_eq!(
            command.note.categories,
            vec!["Work".to_owned(), "Personal".to_owned()]
        );
    }

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
