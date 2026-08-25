use crate::wbxml::{token::Token, Document, Node};

pub mod folder_hierarchy {
    use super::Token;

    pub const PAGE: u8 = 7;
    // Full table verified directly against [MS-ASWBXML] v20250520 section
    // 2.1.2.1.8 (Code Page 7: FolderHierarchy), not reconstructed from
    // memory -- Folders/Folder (legacy GetHierarchy-only) and
    // Delete/Update/FolderCreate/FolderDelete/FolderUpdate added this
    // pass; everything else already present matched the primary source
    // exactly (no corrections needed).
    pub const FOLDERS: Token = tag(0x05, false);
    pub const FOLDER: Token = tag(0x06, false);
    pub const DISPLAY_NAME: Token = tag(0x07, false);
    pub const SERVER_ID: Token = tag(0x08, false);
    pub const PARENT_ID: Token = tag(0x09, false);
    pub const TYPE: Token = tag(0x0a, false);
    pub const STATUS: Token = tag(0x0c, false);
    pub const CHANGES: Token = tag(0x0e, false);
    pub const ADD: Token = tag(0x0f, false);
    pub const DELETE: Token = tag(0x10, false);
    pub const UPDATE: Token = tag(0x11, false);
    pub const SYNC_KEY: Token = tag(0x12, false);
    pub const FOLDER_CREATE: Token = tag(0x13, false);
    pub const FOLDER_DELETE: Token = tag(0x14, false);
    pub const FOLDER_UPDATE: Token = tag(0x15, false);
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

/// MS-ASWBXML codepage 1 (Contacts). Only the fields this gateway
/// actually maps from JSContact are included, not the full ~50-field
/// spec table -- numbering still per spec so adding more later is a
/// drop-in, not a renumbering.
/// MS-ASWBXML codepage 1 (Contacts). The 12 tokens this gateway actually
/// uses (COMPANY_NAME through BUSINESS_PHONE_NUMBER) were all
/// cross-checked against [MS-ASWBXML] v20250520 while adding the rest of
/// this codepage's tokens below and are confirmed correct byte-for-byte.
/// Everything from ANNIVERSARY down is newly scaffolded (unused by
/// `write_contact_add()` today) -- see docs/eas-jmap-type-matrix.md for
/// which of these JSContact (RFC 9553) can actually back.
pub mod contacts {
    use super::Token;

    pub const PAGE: u8 = 1;
    pub const ANNIVERSARY: Token = tag(0x05, false);
    pub const ASSISTANT_NAME: Token = tag(0x06, false);
    pub const ASSISTANT_PHONE_NUMBER: Token = tag(0x07, false);
    pub const BIRTHDAY: Token = tag(0x08, false);
    /// 2.5 only -- 12.0+ uses AirSyncBase:Body (codepage 17) instead.
    pub const BODY: Token = tag(0x09, false);
    /// 2.5 only.
    pub const BODY_SIZE: Token = tag(0x0a, false);
    /// 2.5 only.
    pub const BODY_TRUNCATED: Token = tag(0x0b, false);
    pub const BUSINESS_2_PHONE_NUMBER: Token = tag(0x0c, false);
    pub const BUSINESS_ADDRESS_CITY: Token = tag(0x0d, false);
    pub const BUSINESS_ADDRESS_COUNTRY: Token = tag(0x0e, false);
    pub const BUSINESS_ADDRESS_POSTAL_CODE: Token = tag(0x0f, false);
    pub const BUSINESS_ADDRESS_STATE: Token = tag(0x10, false);
    pub const BUSINESS_ADDRESS_STREET: Token = tag(0x11, false);
    pub const BUSINESS_FAX_NUMBER: Token = tag(0x12, false);
    pub const BUSINESS_PHONE_NUMBER: Token = tag(0x13, false);
    pub const CAR_PHONE_NUMBER: Token = tag(0x14, false);
    pub const CATEGORIES: Token = tag(0x15, true);
    pub const CATEGORY: Token = tag(0x16, false);
    pub const CHILDREN: Token = tag(0x17, true);
    pub const CHILD: Token = tag(0x18, false);
    pub const COMPANY_NAME: Token = tag(0x19, false);
    pub const DEPARTMENT: Token = tag(0x1a, false);
    pub const EMAIL1_ADDRESS: Token = tag(0x1b, false);
    pub const EMAIL2_ADDRESS: Token = tag(0x1c, false);
    pub const EMAIL3_ADDRESS: Token = tag(0x1d, false);
    pub const FILE_AS: Token = tag(0x1e, false);
    pub const FIRST_NAME: Token = tag(0x1f, false);
    pub const HOME_2_PHONE_NUMBER: Token = tag(0x20, false);
    pub const HOME_ADDRESS_CITY: Token = tag(0x21, false);
    pub const HOME_ADDRESS_COUNTRY: Token = tag(0x22, false);
    pub const HOME_ADDRESS_POSTAL_CODE: Token = tag(0x23, false);
    pub const HOME_ADDRESS_STATE: Token = tag(0x24, false);
    pub const HOME_ADDRESS_STREET: Token = tag(0x25, false);
    pub const HOME_FAX_NUMBER: Token = tag(0x26, false);
    pub const HOME_PHONE_NUMBER: Token = tag(0x27, false);
    pub const JOB_TITLE: Token = tag(0x28, false);
    pub const LAST_NAME: Token = tag(0x29, false);
    pub const MIDDLE_NAME: Token = tag(0x2a, false);
    pub const MOBILE_PHONE_NUMBER: Token = tag(0x2b, false);
    pub const OFFICE_LOCATION: Token = tag(0x2c, false);
    pub const OTHER_ADDRESS_CITY: Token = tag(0x2d, false);
    pub const OTHER_ADDRESS_COUNTRY: Token = tag(0x2e, false);
    pub const OTHER_ADDRESS_POSTAL_CODE: Token = tag(0x2f, false);
    pub const OTHER_ADDRESS_STATE: Token = tag(0x30, false);
    pub const OTHER_ADDRESS_STREET: Token = tag(0x31, false);
    pub const PAGER_NUMBER: Token = tag(0x32, false);
    pub const RADIO_PHONE_NUMBER: Token = tag(0x33, false);
    pub const SPOUSE: Token = tag(0x34, false);
    pub const SUFFIX: Token = tag(0x35, false);
    pub const TITLE: Token = tag(0x36, false);
    pub const WEB_PAGE: Token = tag(0x37, false);
    pub const YOMI_COMPANY_NAME: Token = tag(0x38, false);
    pub const YOMI_FIRST_NAME: Token = tag(0x39, false);
    pub const YOMI_LAST_NAME: Token = tag(0x3a, false);
    pub const PICTURE: Token = tag(0x3c, false);
    /// 14.0, 14.1, 16.0, 16.1 only.
    pub const ALIAS: Token = tag(0x3d, false);
    /// 14.0, 14.1, 16.0, 16.1 only.
    pub const WEIGHTED_RANK: Token = tag(0x3e, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// MS-ASWBXML codepage 4 (Calendar). The 9 tokens this gateway actually
/// uses (ALL_DAY_EVENT through UID) carried a comment saying their
/// numbering was "reconstructed from memory at lower confidence" than
/// every other codepage in this file -- now cross-checked directly
/// against [MS-ASWBXML] v20250520 while adding the rest of this
/// codepage's tokens below, and all 9 are confirmed correct byte-for-
/// byte. Confidence upgraded; the rest of this codepage (RECURRENCE and
/// its 9 sibling fields, ATTENDEES, EXCEPTION, ORGANIZER_EMAIL/NAME,
/// REMINDER, ...) is newly scaffolded and unused by
/// `write_calendar_add()` today -- see docs/eas-jmap-type-matrix.md for
/// the field-by-field JSCalendar (RFC 8984 / jscalendarbis) mapping this
/// would need.
pub mod calendar {
    use super::Token;

    pub const PAGE: u8 = 4;
    pub const TIMEZONE: Token = tag(0x05, false);
    pub const ALL_DAY_EVENT: Token = tag(0x06, false);
    pub const ATTENDEES: Token = tag(0x07, true);
    pub const ATTENDEE: Token = tag(0x08, true);
    pub const ATTENDEE_EMAIL: Token = tag(0x09, false);
    pub const ATTENDEE_NAME: Token = tag(0x0a, false);
    /// 2.5 only -- 12.0+ uses AirSyncBase:Body (codepage 17) instead.
    pub const BODY: Token = tag(0x0b, false);
    /// 2.5 only.
    pub const BODY_TRUNCATED: Token = tag(0x0c, false);
    pub const BUSY_STATUS: Token = tag(0x0d, false);
    pub const CATEGORIES: Token = tag(0x0e, true);
    pub const CATEGORY: Token = tag(0x0f, false);
    pub const DTSTAMP: Token = tag(0x11, false);
    pub const END_TIME: Token = tag(0x12, false);
    pub const EXCEPTION: Token = tag(0x13, true);
    pub const EXCEPTIONS: Token = tag(0x14, true);
    pub const DELETED: Token = tag(0x15, false);
    /// 2.5, 12.0, 12.1, 14.0, 14.1 only -- dropped in 16.0/16.1 with no
    /// listed replacement (unlike the plain Location/Body supersessions
    /// noted elsewhere in this codepage).
    pub const EXCEPTION_START_TIME: Token = tag(0x16, false);
    /// 2.5, 12.0, 12.1, 14.0, 14.1 only -- 16.0+ uses AirSyncBase:Location
    /// (codepage 17) instead.
    pub const LOCATION: Token = tag(0x17, false);
    pub const MEETING_STATUS: Token = tag(0x18, false);
    pub const ORGANIZER_EMAIL: Token = tag(0x19, false);
    pub const ORGANIZER_NAME: Token = tag(0x1a, false);
    pub const RECURRENCE: Token = tag(0x1b, true);
    pub const RECURRENCE_TYPE: Token = tag(0x1c, false);
    pub const RECURRENCE_UNTIL: Token = tag(0x1d, false);
    pub const RECURRENCE_OCCURRENCES: Token = tag(0x1e, false);
    pub const RECURRENCE_INTERVAL: Token = tag(0x1f, false);
    pub const RECURRENCE_DAY_OF_WEEK: Token = tag(0x20, false);
    pub const RECURRENCE_DAY_OF_MONTH: Token = tag(0x21, false);
    pub const RECURRENCE_WEEK_OF_MONTH: Token = tag(0x22, false);
    pub const RECURRENCE_MONTH_OF_YEAR: Token = tag(0x23, false);
    pub const REMINDER: Token = tag(0x24, false);
    pub const SENSITIVITY: Token = tag(0x25, false);
    pub const SUBJECT: Token = tag(0x26, false);
    pub const START_TIME: Token = tag(0x27, false);
    pub const UID: Token = tag(0x28, false);
    /// 12.0+ only.
    pub const ATTENDEE_STATUS: Token = tag(0x29, false);
    /// 12.0+ only.
    pub const ATTENDEE_TYPE: Token = tag(0x2a, false);
    /// 14.0+ only.
    pub const DISALLOW_NEW_TIME_PROPOSAL: Token = tag(0x33, false);
    /// 14.0+ only.
    pub const RESPONSE_REQUESTED: Token = tag(0x34, false);
    /// 14.0+ only.
    pub const APPOINTMENT_REPLY_TIME: Token = tag(0x35, false);
    /// 14.0+ only.
    pub const RESPONSE_TYPE: Token = tag(0x36, false);
    /// 14.0+ only.
    pub const CALENDAR_TYPE: Token = tag(0x37, false);
    /// 14.0+ only.
    pub const IS_LEAP_MONTH: Token = tag(0x38, false);
    /// 14.1+ only.
    pub const FIRST_DAY_OF_WEEK: Token = tag(0x39, false);
    /// 14.1+ only.
    pub const ONLINE_MEETING_CONF_LINK: Token = tag(0x3a, false);
    /// 14.1+ only.
    pub const ONLINE_MEETING_EXTERNAL_LINK: Token = tag(0x3b, false);
    /// 16.0+ only.
    pub const CLIENT_UID: Token = tag(0x3c, false);

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
    // Empty-presence boolean flag (Start immediately followed by End, no
    // text) -- signals the client that this Collection's response didn't
    // cover everything; a spec-required tag this gateway never sent.
    pub const MORE_AVAILABLE: Token = tag(0x14, false);
    pub const WINDOW_SIZE: Token = tag(0x15, false);
    pub const OPTIONS: Token = tag(0x17, true);
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
    // BodyPreference itself is a container (Sync request Options only) --
    // its Type/TruncationSize children reuse the SAME tag bytes as the
    // response-side Body's own Type (0x06); WBXML disambiguates by parent
    // context, not the tag byte, so one TYPE constant serves both.
    pub const BODY_PREFERENCE: Token = tag(0x05, true);
    pub const TYPE: Token = tag(0x06, false);
    pub const TRUNCATION_SIZE: Token = tag(0x07, false);
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
    pub const CONTENT_TYPE: Token = tag(0x17, false);
    pub const PREVIEW: Token = tag(0x18, false);
    pub const ATTACHMENTS: Token = tag(0x0e, false);
    pub const ATTACHMENT: Token = tag(0x0f, false);
    pub const DISPLAY_NAME: Token = tag(0x10, false);
    pub const FILE_REFERENCE: Token = tag(0x11, false);
    pub const METHOD: Token = tag(0x12, false);
    pub const IS_INLINE: Token = tag(0x15, false);

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

// ---------------------------------------------------------------------
// Token-only scaffolding below: codepages for commands this gateway does
// NOT implement yet (MeetingResponse/Tasks/ResolveRecipients/ValidateCert/
// Search/GAL/Find), plus supplementary codepages for classes that ARE
// implemented but only partially (Contacts2, Email2, RightsManagement,
// DocumentLibrary). Every token BYTE below is transcribed directly from
// [MS-ASWBXML] v20250520 (the PDF at
// https://officeprotocoldoc.z19.web.core.windows.net/files/MS-ASWBXML/
// [MS-ASWBXML].pdf), section 2.1.2.1 -- not guessed. See
// docs/eas-jmap-command-matrix.md and docs/eas-jmap-type-matrix.md for
// what each of these would need to actually get wired up.
//
// `has_content` on every constant below is an INFERRED guess (container
// vs. leaf, from each element's ordinary XML role), NOT independently
// confirmed against a live encode or a second source the way the
// token-byte values above it are -- unlike the byte value, this field
// isn't given by the WBXML spec's own per-codepage table at all (it only
// lists tag name / token / protocol versions). It also isn't functionally
// load-bearing today: DocumentBuilder::start()/leaf() already force
// has_content to true/false themselves regardless of what a constant
// declares (see the note on this in the test module below), so this is
// documentation for a future implementer, not a currently-exercised
// value. Confirm it against a real request/response before relying on it.
// ---------------------------------------------------------------------

/// WBXML codepage 8 (MeetingResponse), [MS-ASCMD] section 2.2.1.11. The
/// core fields (through UserResponse) are "All" protocol versions; the
/// rest are 14.1+ (InstanceId) or 16.0/16.1 (ProposedStartTime/
/// ProposedEndTime/SendResponse) -- this gateway currently advertises
/// SUPPORTED_PROTOCOLS "12.0,12.1,14.0" only (see activesync.rs), so a
/// real device would never send those three fields against this gateway
/// as it stands today.
pub mod meeting_response {
    use super::Token;

    pub const PAGE: u8 = 8;
    pub const CALENDAR_ID: Token = tag(0x05, false);
    pub const COLLECTION_ID: Token = tag(0x06, false);
    pub const MEETING_RESPONSE: Token = tag(0x07, true);
    pub const REQUEST_ID: Token = tag(0x08, false);
    pub const REQUEST: Token = tag(0x09, true);
    pub const RESULT: Token = tag(0x0a, true);
    pub const STATUS: Token = tag(0x0b, false);
    pub const USER_RESPONSE: Token = tag(0x0c, false);
    /// 14.1, 16.0, 16.1 only.
    pub const INSTANCE_ID: Token = tag(0x0e, false);
    /// 16.1 only.
    pub const PROPOSED_START_TIME: Token = tag(0x10, false);
    /// 16.1 only.
    pub const PROPOSED_END_TIME: Token = tag(0x11, false);
    /// 16.0, 16.1 only.
    pub const SEND_RESPONSE: Token = tag(0x12, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// WBXML codepage 9 (Tasks), [MS-ASTASK]. Shares its recurrence-field
/// shape (Type/Start/Until/Occurrences/Interval/DayOfMonth/DayOfWeek/
/// WeekOfMonth/MonthOfYear/...) with Calendar's own recurrence codepage --
/// see docs/eas-jmap-type-matrix.md for the field-by-field JSCalendar
/// mapping, which both this and Calendar's recurrence would share. Note
/// 1 in the spec: for protocol versions 12.0+, the Body element actually
/// used is AirSyncBase's (codepage 17), not Tasks' own 0x05 -- 0x05 here
/// is 2.5-only.
pub mod tasks {
    use super::Token;

    pub const PAGE: u8 = 9;
    /// 2.5 only -- 12.0+ uses AirSyncBase:Body (codepage 17) instead.
    pub const BODY: Token = tag(0x05, false);
    /// 2.5 only.
    pub const BODY_SIZE: Token = tag(0x06, false);
    /// 2.5 only.
    pub const BODY_TRUNCATED: Token = tag(0x07, false);
    pub const CATEGORIES: Token = tag(0x08, true);
    pub const CATEGORY: Token = tag(0x09, false);
    pub const COMPLETE: Token = tag(0x0a, false);
    pub const DATE_COMPLETED: Token = tag(0x0b, false);
    pub const DUE_DATE: Token = tag(0x0c, false);
    pub const UTC_DUE_DATE: Token = tag(0x0d, false);
    pub const IMPORTANCE: Token = tag(0x0e, false);
    pub const RECURRENCE: Token = tag(0x0f, true);
    pub const TYPE: Token = tag(0x10, false);
    pub const START: Token = tag(0x11, false);
    pub const UNTIL: Token = tag(0x12, false);
    pub const OCCURRENCES: Token = tag(0x13, false);
    pub const INTERVAL: Token = tag(0x14, false);
    pub const DAY_OF_MONTH: Token = tag(0x15, false);
    pub const DAY_OF_WEEK: Token = tag(0x16, false);
    pub const WEEK_OF_MONTH: Token = tag(0x17, false);
    pub const MONTH_OF_YEAR: Token = tag(0x18, false);
    pub const REGENERATE: Token = tag(0x19, false);
    pub const DEAD_OCCUR: Token = tag(0x1a, false);
    pub const REMINDER_SET: Token = tag(0x1b, false);
    pub const REMINDER_TIME: Token = tag(0x1c, false);
    pub const SENSITIVITY: Token = tag(0x1d, false);
    pub const START_DATE: Token = tag(0x1e, false);
    pub const UTC_START_DATE: Token = tag(0x1f, false);
    pub const SUBJECT: Token = tag(0x20, false);
    /// 12.0, 12.1, 14.0, 14.1, 16.0, 16.1 only.
    pub const ORDINAL_DATE: Token = tag(0x22, false);
    /// 12.0, 12.1, 14.0, 14.1, 16.0, 16.1 only.
    pub const SUB_ORDINAL_DATE: Token = tag(0x23, false);
    /// 14.0, 14.1, 16.0, 16.1 only.
    pub const CALENDAR_TYPE: Token = tag(0x24, false);
    /// 14.0, 14.1, 16.0, 16.1 only.
    pub const IS_LEAP_MONTH: Token = tag(0x25, false);
    /// 14.1, 16.0, 16.1 only.
    pub const FIRST_DAY_OF_WEEK: Token = tag(0x26, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// WBXML codepage 10 (ResolveRecipients), [MS-ASCMD] section 2.2.1.15.
pub mod resolve_recipients {
    use super::Token;

    pub const PAGE: u8 = 10;
    pub const RESOLVE_RECIPIENTS: Token = tag(0x05, true);
    pub const RESPONSE: Token = tag(0x06, true);
    pub const STATUS: Token = tag(0x07, false);
    pub const TYPE: Token = tag(0x08, false);
    pub const RECIPIENT: Token = tag(0x09, true);
    pub const DISPLAY_NAME: Token = tag(0x0a, false);
    pub const EMAIL_ADDRESS: Token = tag(0x0b, false);
    pub const CERTIFICATES: Token = tag(0x0c, true);
    pub const CERTIFICATE: Token = tag(0x0d, false);
    pub const MINI_CERTIFICATE: Token = tag(0x0e, false);
    pub const OPTIONS: Token = tag(0x0f, true);
    pub const TO: Token = tag(0x10, false);
    pub const CERTIFICATE_RETRIEVAL: Token = tag(0x11, false);
    pub const RECIPIENT_COUNT: Token = tag(0x12, false);
    pub const MAX_CERTIFICATES: Token = tag(0x13, false);
    pub const MAX_AMBIGUOUS_RECIPIENTS: Token = tag(0x14, false);
    pub const CERTIFICATE_COUNT: Token = tag(0x15, false);
    /// 14.0, 14.1, 16.0, 16.1 only.
    pub const AVAILABILITY: Token = tag(0x16, true);
    /// 14.0, 14.1, 16.0, 16.1 only.
    pub const START_TIME: Token = tag(0x17, false);
    /// 14.0, 14.1, 16.0, 16.1 only.
    pub const END_TIME: Token = tag(0x18, false);
    /// 14.0, 14.1, 16.0, 16.1 only.
    pub const MERGED_FREE_BUSY: Token = tag(0x19, false);
    /// 14.1, 16.0, 16.1 only.
    pub const PICTURE: Token = tag(0x1a, true);
    /// 14.1, 16.0, 16.1 only.
    pub const MAX_SIZE: Token = tag(0x1b, false);
    /// 14.1, 16.0, 16.1 only.
    pub const DATA: Token = tag(0x1c, false);
    /// 14.1, 16.0, 16.1 only.
    pub const MAX_PICTURES: Token = tag(0x1d, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// WBXML codepage 11 (ValidateCert), [MS-ASCMD] section 2.2.1.22.
pub mod validate_cert {
    use super::Token;

    pub const PAGE: u8 = 11;
    pub const VALIDATE_CERT: Token = tag(0x05, true);
    pub const CERTIFICATES: Token = tag(0x06, true);
    pub const CERTIFICATE: Token = tag(0x07, false);
    pub const CERTIFICATE_CHAIN: Token = tag(0x08, true);
    pub const CHECK_CRL: Token = tag(0x09, false);
    pub const STATUS: Token = tag(0x0a, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// WBXML codepage 12 (Contacts2), [MS-ASCNTC] -- supplementary fields for
/// the already-implemented Contacts class (codepage 1). Not wired into
/// `write_contact_add()` yet.
pub mod contacts2 {
    use super::Token;

    pub const PAGE: u8 = 12;
    pub const CUSTOMER_ID: Token = tag(0x05, false);
    pub const GOVERNMENT_ID: Token = tag(0x06, false);
    pub const IM_ADDRESS: Token = tag(0x07, false);
    pub const IM_ADDRESS_2: Token = tag(0x08, false);
    pub const IM_ADDRESS_3: Token = tag(0x09, false);
    pub const MANAGER_NAME: Token = tag(0x0a, false);
    pub const COMPANY_MAIN_PHONE: Token = tag(0x0b, false);
    pub const ACCOUNT_NAME: Token = tag(0x0c, false);
    pub const NICK_NAME: Token = tag(0x0d, false);
    pub const MMS: Token = tag(0x0e, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// WBXML codepage 15 (Search), [MS-ASCMD] section 2.2.1.16. Mailbox/
/// document-library search -- see codepage 16 (GAL) below for the
/// directory/people-search half of the same `Search` command. 0x06,
/// 0x16, 0x1c, 0x1d are gaps in the spec's own token table (not used by
/// any current tag), not omissions here.
pub mod search {
    use super::Token;

    pub const PAGE: u8 = 15;
    pub const SEARCH: Token = tag(0x05, true);
    pub const STORE: Token = tag(0x07, true);
    pub const NAME: Token = tag(0x08, false);
    pub const QUERY: Token = tag(0x09, false);
    pub const OPTIONS: Token = tag(0x0a, true);
    pub const RANGE: Token = tag(0x0b, false);
    pub const STATUS: Token = tag(0x0c, false);
    pub const RESPONSE: Token = tag(0x0d, true);
    pub const RESULT: Token = tag(0x0e, true);
    pub const PROPERTIES: Token = tag(0x0f, true);
    pub const TOTAL: Token = tag(0x10, false);
    /// 12.0+ only.
    pub const EQUAL_TO: Token = tag(0x11, true);
    /// 12.0+ only.
    pub const VALUE: Token = tag(0x12, false);
    /// 12.0+ only.
    pub const AND: Token = tag(0x13, true);
    /// 12.0+ only.
    pub const OR: Token = tag(0x14, true);
    /// 12.0+ only.
    pub const FREE_TEXT: Token = tag(0x15, false);
    /// 12.0+ only.
    pub const DEEP_TRAVERSAL: Token = tag(0x17, false);
    /// 12.0+ only.
    pub const LONG_ID: Token = tag(0x18, false);
    /// 12.0+ only.
    pub const REBUILD_RESULTS: Token = tag(0x19, false);
    /// 12.0+ only.
    pub const LESS_THAN: Token = tag(0x1a, true);
    /// 12.0+ only.
    pub const GREATER_THAN: Token = tag(0x1b, true);
    /// 12.1+ only.
    pub const USER_NAME: Token = tag(0x1e, false);
    /// 12.1+ only.
    pub const PASSWORD: Token = tag(0x1f, false);
    /// 14.0+ only.
    pub const CONVERSATION_ID: Token = tag(0x20, false);
    /// 14.1+ only.
    pub const PICTURE: Token = tag(0x21, true);
    /// 14.1+ only.
    pub const MAX_SIZE: Token = tag(0x22, false);
    /// 14.1+ only.
    pub const MAX_PICTURES: Token = tag(0x23, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// WBXML codepage 16 (GAL), [MS-ASCMD] section 2.2.1.16 -- the directory/
/// people-search result shape for a `Search` command whose `Store>Name`
/// is `"GAL"`, as opposed to codepage 15's mailbox/document-library
/// result shape.
pub mod gal {
    use super::Token;

    pub const PAGE: u8 = 16;
    pub const DISPLAY_NAME: Token = tag(0x05, false);
    pub const PHONE: Token = tag(0x06, false);
    pub const OFFICE: Token = tag(0x07, false);
    pub const TITLE: Token = tag(0x08, false);
    pub const COMPANY: Token = tag(0x09, false);
    pub const ALIAS: Token = tag(0x0a, false);
    pub const FIRST_NAME: Token = tag(0x0b, false);
    pub const LAST_NAME: Token = tag(0x0c, false);
    pub const HOME_PHONE: Token = tag(0x0d, false);
    pub const MOBILE_PHONE: Token = tag(0x0e, false);
    pub const EMAIL_ADDRESS: Token = tag(0x0f, false);
    /// 14.1, 16.0, 16.1 only.
    pub const PICTURE: Token = tag(0x10, true);
    /// 14.1, 16.0, 16.1 only.
    pub const STATUS: Token = tag(0x11, false);
    /// 14.1, 16.0, 16.1 only.
    pub const DATA: Token = tag(0x12, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// WBXML codepage 19 (DocumentLibrary), [MS-ASDOC] -- SharePoint/UNC
/// document search and retrieval. Out of scope for this gateway (no
/// SharePoint/UNC backend exists or is planned); scaffolded for
/// completeness only, per the spec table, all versions 12.0+.
pub mod document_library {
    use super::Token;

    pub const PAGE: u8 = 19;
    pub const LINK_ID: Token = tag(0x05, false);
    pub const DISPLAY_NAME: Token = tag(0x06, false);
    pub const IS_FOLDER: Token = tag(0x07, false);
    pub const CREATION_DATE: Token = tag(0x08, false);
    pub const LAST_MODIFIED_DATE: Token = tag(0x09, false);
    pub const IS_HIDDEN: Token = tag(0x0a, false);
    pub const CONTENT_LENGTH: Token = tag(0x0b, false);
    pub const CONTENT_TYPE: Token = tag(0x0c, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// WBXML codepage 22 (Email2), [MS-ASEMAIL] -- supplementary fields for
/// the already-implemented Email class (codepage 2). `CONVERSATION_ID`/
/// `CONVERSATION_INDEX` are exactly what the gap-analysis doc's reverted
/// conversation-threading attempt (commit `d199d37`) needs -- confirmed
/// present and correctly-numbered here, so a redo has real token values
/// to build on rather than needing to re-derive them.
pub mod email2 {
    use super::Token;

    pub const PAGE: u8 = 22;
    /// 14.0+ only.
    pub const UM_CALLER_ID: Token = tag(0x05, false);
    /// 14.0+ only.
    pub const UM_USER_NOTES: Token = tag(0x06, false);
    /// 14.0+ only.
    pub const UM_ATT_DURATION: Token = tag(0x07, false);
    /// 14.0+ only.
    pub const UM_ATT_ORDER: Token = tag(0x08, false);
    /// 14.0+ only.
    pub const CONVERSATION_ID: Token = tag(0x09, false);
    /// 14.0+ only.
    pub const CONVERSATION_INDEX: Token = tag(0x0a, false);
    /// 14.0+ only.
    pub const LAST_VERB_EXECUTED: Token = tag(0x0b, false);
    /// 14.0+ only.
    pub const LAST_VERB_EXECUTION_TIME: Token = tag(0x0c, false);
    /// 14.0+ only.
    pub const RECEIVED_AS_BCC: Token = tag(0x0d, false);
    /// 14.0+ only.
    pub const SENDER: Token = tag(0x0e, false);
    /// 14.0+ only.
    pub const CALENDAR_TYPE: Token = tag(0x0f, false);
    /// 14.0+ only.
    pub const IS_LEAP_MONTH: Token = tag(0x10, false);
    /// 14.1+ only.
    pub const ACCOUNT_ID: Token = tag(0x11, false);
    /// 14.1+ only.
    pub const FIRST_DAY_OF_WEEK: Token = tag(0x12, false);
    /// 14.1+ only.
    pub const MEETING_MESSAGE_TYPE: Token = tag(0x13, false);
    /// 16.0+ only.
    pub const IS_DRAFT: Token = tag(0x15, false);
    /// 16.0+ only.
    pub const BCC: Token = tag(0x16, false);
    /// 16.0+ only.
    pub const SEND: Token = tag(0x17, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// WBXML codepage 24 (RightsManagement), [MS-ASRM] -- IRM/rights-managed
/// email. All fields 14.1+. Out of scope per the gap-analysis doc
/// (enterprise feature, not relevant to a personal account); scaffolded
/// for completeness only.
pub mod rights_management {
    use super::Token;

    pub const PAGE: u8 = 24;
    pub const RIGHTS_MANAGEMENT_SUPPORT: Token = tag(0x05, false);
    pub const RIGHTS_MANAGEMENT_TEMPLATES: Token = tag(0x06, true);
    pub const RIGHTS_MANAGEMENT_TEMPLATE: Token = tag(0x07, true);
    pub const RIGHTS_MANAGEMENT_LICENSE: Token = tag(0x08, true);
    pub const EDIT_ALLOWED: Token = tag(0x09, false);
    pub const REPLY_ALLOWED: Token = tag(0x0a, false);
    pub const REPLY_ALL_ALLOWED: Token = tag(0x0b, false);
    pub const FORWARD_ALLOWED: Token = tag(0x0c, false);
    pub const MODIFY_RECIPIENTS_ALLOWED: Token = tag(0x0d, false);
    pub const EXTRACT_ALLOWED: Token = tag(0x0e, false);
    pub const PRINT_ALLOWED: Token = tag(0x0f, false);
    pub const EXPORT_ALLOWED: Token = tag(0x10, false);
    pub const PROGRAMMATIC_ACCESS_ALLOWED: Token = tag(0x11, false);
    pub const OWNER: Token = tag(0x12, false);
    pub const CONTENT_EXPIRY_DATE: Token = tag(0x13, false);
    pub const TEMPLATE_ID: Token = tag(0x14, false);
    pub const TEMPLATE_NAME: Token = tag(0x15, false);
    pub const TEMPLATE_DESCRIPTION: Token = tag(0x16, false);
    pub const CONTENT_OWNER: Token = tag(0x17, false);
    pub const REMOVE_RIGHTS_MANAGEMENT_PROTECTION: Token = tag(0x18, false);

    pub const fn tag(token: u8, has_content: bool) -> Token {
        Token {
            code_page: PAGE,
            token,
            has_content,
            has_attributes: false,
        }
    }
}

/// WBXML codepage 25 (Find), [MS-ASCMD] section 2.2.1.2. Real bug-in-
/// waiting if this is ever wired up without checking this comment first:
/// EVERY tag in this codepage is gated to protocol version 16.1 ONLY
/// (confirmed directly in the spec's own per-tag version column, not
/// inferred) -- and this gateway currently advertises
/// `SUPPORTED_PROTOCOLS = "12.0,12.1,14.0"` (see activesync.rs), a
/// deliberate cap put in place this session after the whole
/// over-advertisement root-cause saga. A real device will never invoke
/// Find against this gateway as it stands today no matter how complete
/// an implementation is written, unless/until 16.1 gets re-added to that
/// advertised list -- which is its own decision with its own risk, not
/// a prerequisite to casually bundle in with implementing Find itself.
pub mod find {
    use super::Token;

    pub const PAGE: u8 = 25;
    pub const FIND: Token = tag(0x05, true);
    pub const SEARCH_ID: Token = tag(0x06, false);
    pub const EXECUTE_SEARCH: Token = tag(0x07, true);
    pub const MAILBOX_SEARCH_CRITERION: Token = tag(0x08, true);
    pub const QUERY: Token = tag(0x09, false);
    pub const STATUS: Token = tag(0x0a, false);
    pub const FREE_TEXT: Token = tag(0x0b, false);
    pub const OPTIONS: Token = tag(0x0c, true);
    pub const RANGE: Token = tag(0x0d, false);
    pub const DEEP_TRAVERSAL: Token = tag(0x0e, false);
    pub const RESPONSE: Token = tag(0x11, true);
    pub const RESULT: Token = tag(0x12, true);
    pub const PROPERTIES: Token = tag(0x13, true);
    pub const PREVIEW: Token = tag(0x14, false);
    pub const HAS_ATTACHMENTS: Token = tag(0x15, false);
    pub const TOTAL: Token = tag(0x16, false);
    pub const DISPLAY_CC: Token = tag(0x17, false);
    pub const DISPLAY_BCC: Token = tag(0x18, false);
    pub const GAL_SEARCH_CRITERION: Token = tag(0x19, true);
    pub const MAX_PICTURES: Token = tag(0x20, false);
    pub const MAX_SIZE: Token = tag(0x21, false);
    pub const PICTURE: Token = tag(0x22, true);

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
    /// AirSyncBase:BodyPreference > Type, if the client sent one (real
    /// EAS clients always do on the list-sync Options; a bare
    /// scripted/test client typically doesn't). 1 = plain text, 2 = HTML.
    pub body_pref_type: Option<u8>,
    /// AirSyncBase:BodyPreference > TruncationSize, in characters.
    pub body_pref_truncation_size: Option<usize>,
}

impl Default for SyncCollectionRequest {
    fn default() -> Self {
        Self {
            sync_key: "0".to_owned(),
            collection_id: String::new(),
            window_size: 25,
            get_changes: true,
            commands: Vec::new(),
            body_pref_type: None,
            body_pref_truncation_size: None,
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
    pub contact: ContactFields,
    pub calendar: CalendarFields,
}

/// ActiveSync Calendar class fields decoded from one Add/Change command's
/// ApplicationData, limited to the non-recurring subset this gateway
/// round-trips on read (see `write_calendar_add`) -- recurrence,
/// attendees, and reminders are explicitly out of scope, same as the
/// read path. `start_time`/`end_time` arrive already in EAS's compact
/// UTC DateTime form (`YYYYMMDDTHHMMSSZ`) -- the client sends this
/// natively, no timezone conversion needed on the way in (unlike the
/// read path's `local_to_utc_eas`, which exists because JSCalendar's
/// `start` is LOCAL + a separate `timeZone`, not because EAS itself
/// needs one).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CalendarFields {
    pub subject: Option<String>,
    pub location: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub all_day_event: Option<bool>,
}

/// ActiveSync Contacts class fields decoded from one Add/Change command's
/// ApplicationData, limited to the subset this gateway round-trips (see
/// `write_contact_add` in activesync.rs) -- the same fields, same reason:
/// no round-trip demand yet for the rest of MS-ASCNTC's much larger
/// field set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactFields {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub file_as: Option<String>,
    pub email1_address: Option<String>,
    pub email2_address: Option<String>,
    pub email3_address: Option<String>,
    pub mobile_phone_number: Option<String>,
    pub home_phone_number: Option<String>,
    pub business_phone_number: Option<String>,
    pub company_name: Option<String>,
    pub job_title: Option<String>,
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
                            contact: ContactFields::default(),
                            calendar: CalendarFields::default(),
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
                        } else if token.code_page == airsync_base::PAGE
                            && token.token == airsync_base::TYPE.token
                        {
                            collection.body_pref_type = text.parse().ok();
                        } else if token.code_page == airsync_base::PAGE
                            && token.token == airsync_base::TRUNCATION_SIZE.token
                        {
                            collection.body_pref_truncation_size = text.parse().ok();
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
                        command.contact =
                            extract_contact_fields(&document.nodes[start + 1..idx]);
                        command.calendar =
                            extract_calendar_fields(&document.nodes[start + 1..idx]);
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

/// Same shape as `extract_note_fields`, for the Contacts class instead --
/// simpler since every field this gateway round-trips is a flat leaf, no
/// nesting/repetition (unlike Notes:Categories) to track.
fn extract_contact_fields(nodes: &[Node]) -> ContactFields {
    let mut fields = ContactFields::default();
    let mut path: Vec<Token> = Vec::new();

    for node in nodes {
        match node {
            Node::Start(token) => path.push(*token),
            Node::Text(text) => {
                let Some(&top) = path.last() else { continue };
                if same_token(top, contacts::FIRST_NAME) {
                    fields.first_name = Some(text.clone());
                } else if same_token(top, contacts::LAST_NAME) {
                    fields.last_name = Some(text.clone());
                } else if same_token(top, contacts::FILE_AS) {
                    fields.file_as = Some(text.clone());
                } else if same_token(top, contacts::EMAIL1_ADDRESS) {
                    fields.email1_address = Some(text.clone());
                } else if same_token(top, contacts::EMAIL2_ADDRESS) {
                    fields.email2_address = Some(text.clone());
                } else if same_token(top, contacts::EMAIL3_ADDRESS) {
                    fields.email3_address = Some(text.clone());
                } else if same_token(top, contacts::MOBILE_PHONE_NUMBER) {
                    fields.mobile_phone_number = Some(text.clone());
                } else if same_token(top, contacts::HOME_PHONE_NUMBER) {
                    fields.home_phone_number = Some(text.clone());
                } else if same_token(top, contacts::BUSINESS_PHONE_NUMBER) {
                    fields.business_phone_number = Some(text.clone());
                } else if same_token(top, contacts::COMPANY_NAME) {
                    fields.company_name = Some(text.clone());
                } else if same_token(top, contacts::JOB_TITLE) {
                    fields.job_title = Some(text.clone());
                }
            }
            Node::End => {
                path.pop();
            }
            Node::Opaque(_) => {}
        }
    }

    fields
}

/// Same shape as `extract_contact_fields` -- flat leaves, no nesting for
/// the fields this gateway round-trips.
fn extract_calendar_fields(nodes: &[Node]) -> CalendarFields {
    let mut fields = CalendarFields::default();
    let mut path: Vec<Token> = Vec::new();

    for node in nodes {
        match node {
            Node::Start(token) => path.push(*token),
            Node::Text(text) => {
                let Some(&top) = path.last() else { continue };
                if same_token(top, calendar::SUBJECT) {
                    fields.subject = Some(text.clone());
                } else if same_token(top, calendar::LOCATION) {
                    fields.location = Some(text.clone());
                } else if same_token(top, calendar::START_TIME) {
                    fields.start_time = Some(text.clone());
                } else if same_token(top, calendar::END_TIME) {
                    fields.end_time = Some(text.clone());
                } else if same_token(top, calendar::ALL_DAY_EVENT) {
                    fields.all_day_event = Some(text != "0");
                }
            }
            Node::End => {
                path.pop();
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

    /// A leaf whose content is raw binary (WBXML opaque data), not text --
    /// e.g. `Email2:ConversationId`, which EAS spec defines as opaque
    /// bytes, not a string.
    pub fn opaque_leaf(&mut self, token: Token, data: impl Into<bytes::Bytes>) {
        let mut token = token;
        token.has_content = true;
        self.nodes.push(Node::Start(token));
        self.nodes.push(Node::Opaque(data.into()));
        self.nodes.push(Node::End);
    }

    pub fn end(&mut self) {
        self.nodes.push(Node::End);
    }

    /// A genuinely content-less, attribute-less marker tag -- just the
    /// bare tag byte, no matching End. Distinct from `start()`
    /// immediately followed by `end()`, which forces has_content=true and
    /// DOES emit an End byte. Real bug, found live: Oof's
    /// AppliesToInternal/AppliesToExternalKnown/AppliesToExternalUnknown
    /// are pure presence-flag elements (mutually exclusive, no content of
    /// their own -- see [MS-ASCMD]'s OofMessage reference: "The presence
    /// of one of the following elements... indicates the audience").
    /// Encoding them as start+end (content bit set, immediately closed)
    /// produced a real device-side WBXML parse error, confirmed via
    /// idevicesyslog: "We have an int in our WBXML, but Exchange never
    /// gives us this. Parse error." -- "Object is <private>, codePage
    /// 0x12 token 0xe" (0x12=18=Settings, 0xe=AppliesToInternal).
    /// exchangesyncd's parser apparently falls through to a generic/
    /// int-guessing content handler when it sees the content bit set on a
    /// tag it expects to be a bare marker with no content at all.
    pub fn empty_tag(&mut self, token: Token) {
        let mut token = token;
        token.has_content = false;
        self.nodes.push(Node::Start(token));
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
    use crate::wbxml::encode_document;

    #[test]
    fn empty_tag_encodes_a_bare_content_less_byte_with_no_matching_end() {
        // Real bug, confirmed live via idevicesyslog: encoding Oof's
        // AppliesToInternal as start()+end() (content bit set, closed
        // immediately) produced a real device-side WBXML parse error --
        // "We have an int in our WBXML, but Exchange never gives us
        // this." at exactly codePage 0x12 (Settings) token 0xe
        // (AppliesToInternal). empty_tag() must produce a single bare
        // byte with the content bit UNSET and no corresponding End node
        // at all -- that's the whole fix.
        let mut builder = DocumentBuilder::new();
        builder.start(settings::OOF_MESSAGE);
        builder.empty_tag(settings::APPLIES_TO_INTERNAL);
        builder.leaf(settings::ENABLED, "1");
        builder.end();
        let doc = builder.finish();

        assert_eq!(
            doc.nodes[1],
            Node::Start(Token {
                code_page: settings::PAGE,
                token: settings::APPLIES_TO_INTERNAL.token,
                has_content: false,
                has_attributes: false,
            })
        );
        // The very next node must be ENABLED's own Start, not an End --
        // proof no matching End was emitted for the empty tag.
        assert!(matches!(doc.nodes[2], Node::Start(_)));

        let encoded = encode_document(&doc);
        // AppliesToInternal's byte must NOT have the content bit (0x40)
        // set: bare tag 0x0e, not 0x4e.
        let applies_to_internal_byte = 0x0eu8;
        assert!(encoded.contains(&applies_to_internal_byte));
        assert!(!encoded.contains(&0x4eu8));
    }

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
    fn parses_contact_change_with_flat_fields() {
        // Mirrors a real device Change: ServerId already assigned (no
        // ClientId), flat Contacts-codepage leaves -- no nesting to
        // resolve here (unlike Notes:Categories), just confirming
        // extract_contact_fields() picks the right tokens out of the
        // same ApplicationData subtree extract_note_fields() also walks.
        let mut builder = DocumentBuilder::new();
        builder.start(airsync::SYNC);
        builder.start(airsync::COLLECTIONS);
        builder.start(airsync::COLLECTION);
        builder.leaf(airsync::SYNC_KEY, "1");
        builder.leaf(airsync::COLLECTION_ID, "ab_x");
        builder.start(airsync::COMMANDS);
        builder.start(airsync::CHANGE);
        builder.leaf(airsync::SERVER_ID, "contact-1");
        builder.start(airsync::APPLICATION_DATA);
        builder.leaf(contacts::FIRST_NAME, "Ada");
        builder.leaf(contacts::LAST_NAME, "Lovelace");
        builder.leaf(contacts::EMAIL1_ADDRESS, "ada@example.com");
        builder.leaf(contacts::MOBILE_PHONE_NUMBER, "555-0100");
        builder.leaf(contacts::COMPANY_NAME, "Analytical Engines Ltd");
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
        assert_eq!(command.kind, SyncClientCommandKind::Change);
        assert_eq!(command.server_id, "contact-1");
        assert_eq!(command.contact.first_name.as_deref(), Some("Ada"));
        assert_eq!(command.contact.last_name.as_deref(), Some("Lovelace"));
        assert_eq!(
            command.contact.email1_address.as_deref(),
            Some("ada@example.com")
        );
        assert_eq!(
            command.contact.mobile_phone_number.as_deref(),
            Some("555-0100")
        );
        assert_eq!(
            command.contact.company_name.as_deref(),
            Some("Analytical Engines Ltd")
        );
        assert_eq!(command.contact.email2_address, None);
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
    fn parses_body_preference_from_a_real_ipad_sync_request() {
        // Real bug, confirmed live via the zoidberg A/B test: a real
        // iPad's Sync request carried exactly this shape (Options >
        // BodyPreference { Type: 1, TruncationSize: 500 }), and it was
        // silently ignored entirely -- the gateway always sent the full
        // untruncated HTML body regardless, which the client rejected
        // every time, so it never advanced SyncKey off "0".
        let mut builder = DocumentBuilder::new();
        builder.start(airsync::SYNC);
        builder.start(airsync::COLLECTIONS);
        builder.start(airsync::COLLECTION);
        builder.leaf(airsync::SYNC_KEY, "0");
        builder.leaf(airsync::COLLECTION_ID, "a");
        builder.start(airsync::OPTIONS);
        builder.start(airsync_base::BODY_PREFERENCE);
        builder.leaf(airsync_base::TYPE, "1");
        builder.leaf(airsync_base::TRUNCATION_SIZE, "500");
        builder.end();
        builder.end();
        builder.end();
        builder.end();
        builder.end();

        let collections = sync_collections(&builder.finish());

        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].body_pref_type, Some(1));
        assert_eq!(collections[0].body_pref_truncation_size, Some(500));
    }

    #[test]
    fn sync_request_with_no_body_preference_leaves_it_unset() {
        let mut builder = DocumentBuilder::new();
        builder.start(airsync::SYNC);
        builder.start(airsync::COLLECTIONS);
        builder.start(airsync::COLLECTION);
        builder.leaf(airsync::SYNC_KEY, "0");
        builder.leaf(airsync::COLLECTION_ID, "a");
        builder.end();
        builder.end();
        builder.end();

        let collections = sync_collections(&builder.finish());

        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].body_pref_type, None);
        assert_eq!(collections[0].body_pref_truncation_size, None);
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
