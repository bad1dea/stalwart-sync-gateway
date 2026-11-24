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
/// MS-ASWBXML codepage 1 (Contacts). Tokens used by this gateway for names,
/// emails, phones, addresses, categories, organization and title were
/// cross-checked against [MS-ASWBXML] v20250520 and are byte-for-byte correct.
/// The remaining tokens are scaffolded; see docs/eas-jmap-type-matrix.md for
/// current coverage and their possible JSContact (RFC 9553) mappings.
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
    // Per the same authoritative WBXML DTD table cited above for
    // RESPONSES (z-push lib/wbxml/wbxmldefs.php, codepage 0): 0x10 =
    // "Class" (0x11 = "Version", not currently used by this gateway).
    // Needed for `Search`'s `airsync:Class` query filter/result element.
    pub const CLASS: Token = tag(0x10, false);
    pub const COLLECTION_ID: Token = tag(0x12, false);
    pub const GET_CHANGES: Token = tag(0x13, false);
    // Empty-presence boolean flag (Start immediately followed by End, no
    // text) -- signals the client that this Collection's response didn't
    // cover everything; a spec-required tag this gateway never sent.
    pub const MORE_AVAILABLE: Token = tag(0x14, false);
    pub const WINDOW_SIZE: Token = tag(0x15, false);
    pub const OPTIONS: Token = tag(0x17, true);
    /// [MS-ASCMD] 2.2.3.68.2 Options > FilterType: 0 all, 1 one day, 2 three
    /// days, 3 one week, 4 two weeks, 5 one month (mail).
    pub const FILTER_TYPE: Token = tag(0x18, false);
    pub const DELETES_AS_MOVES: Token = tag(0x1e, false);
    pub const SOFT_DELETE: Token = tag(0x21, false);
    pub const MIME_SUPPORT: Token = tag(0x22, false);
    pub const WAIT: Token = tag(0x24, false);
    pub const LIMIT: Token = tag(0x25, false);
    pub const PARTIAL: Token = tag(0x26, false);
    pub const HEARTBEAT_INTERVAL: Token = tag(0x29, false);
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
    pub const REPLY_TO: Token = tag(0x19, false);
    pub const FLAG: Token = tag(0x3a, false);
    pub const FLAG_STATUS: Token = tag(0x3b, false);
    pub const COMPLETE_TIME: Token = tag(0x3e, false);
    pub const FLAG_TYPE: Token = tag(0x3d, false);

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
    pub const ALL_OR_NONE: Token = tag(0x08, false);
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
    pub const LOCATION: Token = tag(0x20, true);
    pub const LATITUDE: Token = tag(0x27, false);
    pub const LONGITUDE: Token = tag(0x28, false);
    pub const CONTENT_TYPE: Token = tag(0x17, false);
    pub const PREVIEW: Token = tag(0x18, false);
    pub const ATTACHMENTS: Token = tag(0x0e, false);
    pub const ATTACHMENT: Token = tag(0x0f, false);
    pub const DISPLAY_NAME: Token = tag(0x10, false);
    pub const FILE_REFERENCE: Token = tag(0x11, false);
    pub const METHOD: Token = tag(0x12, false);
    pub const CONTENT_ID: Token = tag(0x13, false);
    pub const CONTENT_LOCATION: Token = tag(0x14, false);
    pub const IS_INLINE: Token = tag(0x15, false);
    pub const ADD: Token = tag(0x1c, true);
    pub const CLIENT_ID: Token = tag(0x1e, false);
    pub const CONTENT: Token = tag(0x1f, false);

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

    // Token values verified against Z-Push's lib/wbxml/wbxmldefs.php (page 0x0d).
    // The previous table was shifted down by one (STATUS was 0x06, which is the
    // unused AutdState tag), so no Ping response ever carried a real Status.
    pub const PAGE: u8 = 13;
    pub const PING: Token = tag(0x05, false);
    pub const AUTD_STATE: Token = tag(0x06, false);
    pub const STATUS: Token = tag(0x07, false);
    pub const LIFETIME: Token = tag(0x08, false);
    pub const FOLDERS: Token = tag(0x09, false);
    pub const FOLDER: Token = tag(0x0a, false);
    pub const SERVER_ENTRY_ID: Token = tag(0x0b, false);
    pub const FOLDER_TYPE: Token = tag(0x0c, false);
    pub const MAX_FOLDERS: Token = tag(0x0d, false);

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
    pub const DEVICE_INFORMATION: Token = tag(0x16, false);
    pub const USER_INFORMATION: Token = tag(0x1d, false);
    pub const EMAIL_ADDRESSES: Token = tag(0x1e, false);
    pub const SMTP_ADDRESS: Token = tag(0x1f, false);
    pub const PRIMARY_SMTP_ADDRESS: Token = tag(0x23, false);
    pub const ACCOUNTS: Token = tag(0x24, false);
    pub const ACCOUNT: Token = tag(0x25, false);
    pub const ACCOUNT_ID: Token = tag(0x26, false);
    pub const ACCOUNT_NAME: Token = tag(0x27, false);
    pub const USER_DISPLAY_NAME: Token = tag(0x28, false);
    pub const SEND_DISABLED: Token = tag(0x29, false);

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
    pub const OPTIONS: Token = tag(0x08, true);
    pub const RANGE: Token = tag(0x09, false);
    pub const TOTAL: Token = tag(0x0a, false);
    pub const DATA: Token = tag(0x0c, false);
    pub const STATUS: Token = tag(0x0d, false);
    pub const RESPONSE: Token = tag(0x0e, false);
    pub const PART: Token = tag(0x11, false);
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
    pub const REPLACE_MIME: Token = tag(0x09, false);
    pub const SOURCE: Token = tag(0x0b, false);
    pub const FOLDER_ID: Token = tag(0x0c, false);
    pub const ITEM_ID: Token = tag(0x0d, false);
    pub const LONG_ID: Token = tag(0x0e, false);
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
/// SUPPORTED_PROTOCOLS "14.0" only (12.0/12.1 until 2026-11-12) (see activesync.rs), so a
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
/// `SUPPORTED_PROTOCOLS = "14.0"` (12.0/12.1 until 2026-11-12) (see activesync.rs), a
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestedBodyPreference {
    pub body_type: u8,
    pub truncation_size: Option<usize>,
    #[serde(default)]
    pub all_or_none: bool,
    pub preview: Option<usize>,
}

pub fn mime_support(document: &Document) -> Result<u8, &'static str> {
    let mut value = None;
    let options = if matches!(document.nodes.first(), Some(Node::Start(t))
        if same_token(*t, airsync::OPTIONS))
    {
        vec![document.clone()]
    } else {
        child_elements(document)
            .into_iter()
            .filter(|child| {
                matches!(child.nodes.first(), Some(Node::Start(t))
                if same_token(*t, airsync::OPTIONS)
                    || same_token(*t, item_operations::OPTIONS)
                    || same_token(*t, search::OPTIONS))
            })
            .collect()
    };
    for options in options {
        for child in child_elements(&options) {
            if matches!(child.nodes.first(), Some(Node::Start(t))
                if same_token(*t, airsync::MIME_SUPPORT))
            {
                let raw =
                    find_text_after(&child, airsync::MIME_SUPPORT).ok_or("empty MIMESupport")?;
                let parsed = match raw {
                    "0" => 0,
                    "1" => 1,
                    "2" => 2,
                    _ => return Err("invalid MIMESupport"),
                };
                if value.replace(parsed).is_some() {
                    return Err("duplicate MIMESupport");
                }
            }
        }
    }
    Ok(value.unwrap_or(0))
}

pub fn body_preferences(document: &Document) -> Result<Vec<RequestedBodyPreference>, &'static str> {
    let mut preferences = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut seen = [false; 5];
    for (index, node) in document.nodes.iter().enumerate() {
        match node {
            Node::Start(token) => {
                if same_token(*token, airsync_base::BODY_PREFERENCE) {
                    if start.is_some() || !token.has_content {
                        return Err("invalid BodyPreference");
                    }
                    start = Some((index, depth));
                }
                if token.has_content {
                    depth += 1;
                }
            }
            Node::End => {
                depth = depth.checked_sub(1).ok_or("invalid body preference tree")?;
                if start.is_some_and(|(_, level)| level == depth) {
                    let (first, _) = start.take().unwrap();
                    let doc = Document {
                        nodes: document.nodes[first..=index].to_vec(),
                    };
                    let fields = child_elements(&doc);
                    let text = |token| -> Result<Option<&str>, &'static str> {
                        let mut matching = fields.iter().filter(|field| {
                            matches!(field.nodes.first(),
                            Some(Node::Start(t)) if same_token(*t, token))
                        });
                        let Some(field) = matching.next() else {
                            return Ok(None);
                        };
                        if matching.next().is_some() {
                            return Err("duplicate body preference field");
                        }
                        find_text_after(field, token)
                            .map(Some)
                            .ok_or("empty body preference field")
                    };
                    let body_type = text(airsync_base::TYPE)?
                        .ok_or("missing body type")?
                        .parse::<u8>()
                        .map_err(|_| "invalid body type")?;
                    if !(1..=4).contains(&body_type) || seen[body_type as usize] {
                        return Err("invalid or duplicate body type");
                    }
                    seen[body_type as usize] = true;
                    let truncation_size = text(airsync_base::TRUNCATION_SIZE)?
                        .map(|value| value.parse::<u32>().map(|value| value as usize))
                        .transpose()
                        .map_err(|_| "invalid truncation size")?;
                    let all_or_none = match text(airsync_base::ALL_OR_NONE)? {
                        None | Some("0" | "false") => false,
                        Some("1" | "true") => true,
                        _ => return Err("invalid AllOrNone"),
                    };
                    let preview = text(airsync_base::PREVIEW)?
                        .map(|value| value.parse::<usize>())
                        .transpose()
                        .map_err(|_| "invalid preview size")?;
                    if preview.is_some_and(|value| value > 255) {
                        return Err("invalid preview size");
                    }
                    preferences.push(RequestedBodyPreference {
                        body_type,
                        truncation_size,
                        all_or_none,
                        preview,
                    });
                }
            }
            _ => {}
        }
    }
    if start.is_some() {
        return Err("unclosed BodyPreference");
    }
    Ok(preferences)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncCollectionRequest {
    pub sync_key: String,
    pub collection_id: String,
    pub window_size: usize,
    pub get_changes: bool,
    pub deletes_as_moves: bool,
    pub commands: Vec<SyncClientCommand>,
    pub options_count: usize,
    pub options_class: Option<String>,
    pub body_preferences: Vec<RequestedBodyPreference>,
    pub mime_support: u8,
    pub body_preferences_valid: bool,
    /// Options > FilterType, if sent this request. Absent means "same as the
    /// previous Sync" ([MS-ASCMD] 2.2.3.125: Options persist).
    pub filter_type: Option<u8>,
}

impl Default for SyncCollectionRequest {
    fn default() -> Self {
        Self {
            sync_key: "0".to_owned(),
            collection_id: String::new(),
            window_size: 100,
            get_changes: true,
            deletes_as_moves: true,
            commands: Vec::new(),
            options_count: 0,
            options_class: None,
            body_preferences: Vec::new(),
            mime_support: 0,
            body_preferences_valid: true,
            filter_type: None,
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
    pub flag_status: Option<u8>,
    pub flag_fields: Vec<(u8, u8, String)>,
    pub flag_valid: bool,
    pub unsupported_mail_change: bool,
    pub document: Document,
    pub note: NoteFields,
    pub contact: ContactFields,
    pub calendar: CalendarFields,
    pub task: TaskFields,
}

/// ActiveSync Calendar class fields decoded from one Add/Change command's
/// ApplicationData, limited to the non-recurring subset this gateway
/// round-trips on read (see `write_calendar_add`) -- recurrence and
/// reminders are explicitly out of scope, same as the read path.
/// Attendees ARE now round-tripped (2026-08-25 -- see `Attendee` doc
/// comment for why this was previously marked out of scope and no
/// longer is). `start_time`/`end_time` arrive already in EAS's compact
/// UTC DateTime form (`YYYYMMDDTHHMMSSZ`) -- the client sends this
/// natively, no timezone conversion needed on the way in (unlike the
/// read path's `local_to_utc_eas`, which exists because JSCalendar's
/// `start` is LOCAL + a separate `timeZone`, not because EAS itself
/// needs one).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CalendarFields {
    pub subject: Option<String>,
    pub location: Option<String>,
    pub location_coordinates: Option<(String, String)>,
    pub unsupported_location: bool,
    pub body: Option<String>,
    pub body_type: Option<u8>,
    pub unsupported_body: bool,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub all_day_event: Option<bool>,
    pub busy_status: Option<u8>,
    pub unsupported_busy_status: bool,
    pub sensitivity: Option<u8>,
    pub unsupported_sensitivity: bool,
    pub reminder: Option<Option<u32>>,
    pub unsupported_reminder: bool,
    pub categories: Option<Vec<String>>,
    pub unsupported_categories: bool,
    pub unsupported_recurrence: bool,
    pub unsupported_response_type: bool,
    pub attendees: Vec<AttendeeField>,
    pub attendees_present: bool,
    pub unsupported_attendees: bool,
}

/// One `calendar:Attendee` from a client Add/Change -- sent when the
/// authenticated user is creating/editing an event AS ORGANIZER and adds
/// invitees. `optional` comes from AttendeeType (2 = Optional per
/// [MS-ASCAL], confirmed against the live spec page, not guessed) --
/// absent or any other value defaults to Required.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttendeeField {
    pub email: String,
    pub name: Option<String>,
    pub optional: bool,
}

/// ActiveSync Tasks class fields decoded from one Add/Change command's
/// ApplicationData, limited to the subset this gateway round-trips (see
/// `write_task_command` in activesync.rs) -- same minimal-non-recurring
/// scoping as `CalendarFields`. Due dates accept EAS dateTime and the
/// compact form used by older clients.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskFields {
    pub subject: Option<String>,
    pub body: Option<String>,
    pub body_type: Option<u8>,
    pub unsupported_body: bool,
    pub due_date: Option<String>,
    pub utc_due_date: Option<String>,
    pub complete: Option<bool>,
    pub date_completed: Option<String>,
}

/// ActiveSync Contacts class fields decoded from one Add/Change command's
/// ApplicationData, limited to the subset this gateway round-trips (see
/// `write_contact_add` in activesync.rs) -- the same fields, same reason:
/// no round-trip demand yet for the rest of MS-ASCNTC's much larger
/// field set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactFields {
    pub first_name: Option<String>,
    pub middle_name: Option<String>,
    pub last_name: Option<String>,
    pub suffix: Option<String>,
    pub name_prefix: Option<String>,
    pub yomi_first_name: Option<String>,
    pub yomi_last_name: Option<String>,
    pub yomi_company_name: Option<String>,
    pub office_location: Option<String>,
    pub nickname: Option<String>,
    pub company_main_phone: Option<String>,
    pub mms: Option<String>,
    pub account_name: Option<String>,
    pub picture: Option<String>,
    pub im_address: Option<String>,
    pub im_address_2: Option<String>,
    pub im_address_3: Option<String>,
    pub birthday: Option<String>,
    pub anniversary: Option<String>,
    pub file_as: Option<String>,
    pub email1_address: Option<String>,
    pub email2_address: Option<String>,
    pub email3_address: Option<String>,
    pub mobile_phone_number: Option<String>,
    pub home_phone_number: Option<String>,
    pub home_2_phone_number: Option<String>,
    pub business_phone_number: Option<String>,
    pub business_2_phone_number: Option<String>,
    pub home_fax_number: Option<String>,
    pub business_fax_number: Option<String>,
    pub pager_number: Option<String>,
    pub car_phone_number: Option<String>,
    pub radio_phone_number: Option<String>,
    pub web_page: Option<String>,
    pub assistant_phone_number: Option<String>,
    pub company_name: Option<String>,
    pub department: Option<String>,
    pub job_title: Option<String>,
    pub business_address_city: Option<String>,
    pub business_address_country: Option<String>,
    pub business_address_postal_code: Option<String>,
    pub business_address_state: Option<String>,
    pub business_address_street: Option<String>,
    pub home_address_city: Option<String>,
    pub home_address_country: Option<String>,
    pub home_address_postal_code: Option<String>,
    pub home_address_state: Option<String>,
    pub home_address_street: Option<String>,
    pub other_address_city: Option<String>,
    pub other_address_country: Option<String>,
    pub other_address_postal_code: Option<String>,
    pub other_address_state: Option<String>,
    pub other_address_street: Option<String>,
    pub categories: Option<Vec<String>>,
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
    pub collection_id: Option<String>,
    pub server_id: Option<String>,
    /// `search:LongId` (MS-ASCMD 2.2.3.98.3, WBXML codepage 15 token
    /// `0x18` -- the SAME token already scaffolded as `search::LONG_ID`;
    /// per the real schema this element is genuinely defined once in the
    /// Search namespace and reused by reference in ItemOperations, not a
    /// separate ItemOperations-local id) -- what a device sends to fetch
    /// a specific `Search` result by the opaque id that result's own
    /// `LongId` carried, instead of the CollectionId+ServerId shape.
    pub long_id: Option<String>,
    pub file_reference: Option<String>,
    pub range: Option<String>,
    pub body_preferences: Vec<RequestedBodyPreference>,
    pub mime_support: u8,
}

/// Direct child elements, retaining their complete WBXML subtrees.
pub fn child_elements(document: &Document) -> Vec<Document> {
    let mut children = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (index, node) in document.nodes.iter().enumerate() {
        match node {
            Node::Start(token) => {
                if depth == 1 {
                    start = index;
                    if !token.has_content {
                        children.push(Document {
                            nodes: vec![node.clone()],
                        });
                    }
                }
                if token.has_content {
                    depth += 1;
                }
            }
            Node::End => {
                depth -= 1;
                if depth == 1 {
                    children.push(Document {
                        nodes: document.nodes[start..=index].to_vec(),
                    });
                }
            }
            _ => {}
        }
    }
    children
}

pub fn item_operations_fetch(document: &Document) -> Option<ItemOperationsFetchRequest> {
    let store = find_text_after(document, item_operations::STORE)?.to_owned();
    let collection_id = find_text_after(document, airsync::COLLECTION_ID).map(str::to_owned);
    let server_id = find_text_after(document, airsync::SERVER_ID).map(str::to_owned);
    let long_id = find_text_after(document, search::LONG_ID).map(str::to_owned);
    let file_reference = find_text_after(document, airsync_base::FILE_REFERENCE).map(str::to_owned);
    if file_reference.is_none()
        && long_id.is_none()
        && (collection_id.is_none() || server_id.is_none())
    {
        return None;
    }
    Some(ItemOperationsFetchRequest {
        store,
        collection_id,
        server_id,
        long_id,
        file_reference,
        range: find_text_after(document, item_operations::RANGE).map(str::to_owned),
        body_preferences: body_preferences(document).ok()?,
        mime_support: mime_support(document).ok()?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub store_name: String,
    /// Optional item-class filter for a Mailbox search.
    pub class: Option<String>,
    pub classes: Vec<String>,
    pub unsupported_class_query: bool,
    pub free_text: Option<String>,
    pub free_texts: Vec<String>,
    pub collection_id: Option<String>,
    pub deep_traversal: bool,
    pub unsupported_query: bool,
    pub conversation_id: Option<uuid::Uuid>,
    pub invalid_conversation: bool,
    pub received_after: Option<String>,
    pub received_before: Option<String>,
    pub or_terms: Option<SearchOrTerms>,
    /// The EAS `Range` element's two numbers, parsed as-is (zero-based,
    /// inclusive on both ends per the real schema's own prose, e.g.
    /// "0-9" = the first 10 results) -- NOT yet converted to a
    /// position/limit pair, since that conversion is the caller's job.
    pub range: Option<(i64, i64)>,
    pub range_error: Option<&'static str>,
    pub body_preferences: Vec<RequestedBodyPreference>,
    pub mime_support: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOrTerms {
    pub texts: Vec<String>,
    pub received_after: Option<String>,
    pub received_before: Option<String>,
}

/// Parses a `Search` request. Real devices send exactly one `Store` per
/// request (the schema technically allows `maxOccurs="unbounded"`, but
/// no real client is known to search multiple stores in one call, same
/// scoping call this codebase already makes elsewhere for similarly
/// theoretical multi-occurrence request shapes) -- `find_text_after`
/// naturally picks up the first (only) occurrence of each element.
pub fn search_request(document: &Document) -> Option<SearchRequest> {
    let store_name = find_text_after(document, search::NAME)?.to_owned();
    let (classes, unsupported_class_query) = search_classes(document);
    let class = classes.first().cloned();
    let free_texts = search_texts(document, false);
    let free_text = free_texts
        .first()
        .cloned()
        .or_else(|| find_text_after(document, search::QUERY).map(str::to_owned));
    let (range, range_error) = match find_text_after(document, search::RANGE) {
        Some(raw) => {
            let parsed = raw.split_once('-').and_then(|(start, end)| {
                if start.is_empty()
                    || end.is_empty()
                    || !start.bytes().all(|b| b.is_ascii_digit())
                    || !end.bytes().all(|b| b.is_ascii_digit())
                {
                    return None;
                }
                let start: i64 = start.parse().ok()?;
                let end: i64 = end.parse().ok()?;
                (end >= start).then_some((start, end))
            });
            match parsed {
                Some((start, end)) if end - start < 100 => (Some((start, end)), None),
                Some(_) => (None, Some("12")),
                None => (None, Some("2")),
            }
        }
        None if contains_token(document, search::RANGE) => (None, Some("2")),
        None => (None, None),
    };
    let body_preferences = body_preferences(document).ok()?;
    let store = child_elements(document).into_iter().find(|child|
        matches!(child.nodes.first(), Some(Node::Start(t)) if same_token(*t, search::STORE)))?;
    let mime_support = mime_support(&store).ok()?;
    let (received_after, after_invalid) =
        search_date_comparison(&store, search::GREATER_THAN, search::AND);
    let (received_before, before_invalid) =
        search_date_comparison(&store, search::LESS_THAN, search::AND);
    let (conversation_id, invalid_conversation) = search_conversation(&store);
    let (or_doc, or_invalid) = search_or_element(&store);
    let (or_after, or_after_invalid) =
        search_date_comparison(&store, search::GREATER_THAN, search::OR);
    let (or_before, or_before_invalid) =
        search_date_comparison(&store, search::LESS_THAN, search::OR);
    let or_terms = or_doc.as_ref().map(|or| SearchOrTerms {
        texts: search_texts(or, true),
        received_after: or_after,
        received_before: or_before,
    });
    Some(SearchRequest {
        store_name,
        class,
        classes,
        unsupported_class_query,
        free_text,
        free_texts,
        collection_id: find_text_after(document, airsync::COLLECTION_ID).map(str::to_owned),
        deep_traversal: contains_token(document, search::DEEP_TRAVERSAL),
        unsupported_query: after_invalid
            || before_invalid
            || or_invalid
            || or_after_invalid
            || or_before_invalid
            || or_terms.as_ref().is_some_and(|terms| {
                terms.texts.iter().all(String::is_empty)
                    && terms.received_after.is_none()
                    && terms.received_before.is_none()
            })
            || contains_token(document, search::EQUAL_TO),
        conversation_id,
        invalid_conversation,
        received_after,
        received_before,
        or_terms,
        range,
        range_error,
        body_preferences,
        mime_support,
    })
}

fn search_classes(document: &Document) -> (Vec<String>, bool) {
    fn visit(
        document: &Document,
        parent: Option<Token>,
        values: &mut Vec<String>,
        invalid: &mut bool,
    ) {
        if let Some(Node::Start(token)) = document.nodes.first() {
            if same_token(*token, airsync::CLASS) {
                if !parent.is_some_and(|parent| same_token(parent, search::AND)) {
                    *invalid = true;
                }
                match document.nodes.as_slice() {
                    [Node::Start(_), Node::Text(value), Node::End] if !value.is_empty() => {
                        values.push(value.clone())
                    }
                    _ => *invalid = true,
                }
            }
            for child in child_elements(document) {
                visit(&child, Some(*token), values, invalid);
            }
        }
    }
    let mut values = Vec::new();
    let mut invalid = false;
    visit(document, None, &mut values, &mut invalid);
    for class in &values {
        if !["Email", "Contacts", "Calendar", "Tasks", "Notes"]
            .iter()
            .any(|supported| class.eq_ignore_ascii_case(supported))
        {
            invalid = true;
        }
    }
    let mut classes: Vec<String> = Vec::new();
    for class in values {
        if !classes
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&class))
        {
            classes.push(class);
        }
    }
    (classes, invalid)
}

fn search_conversation(store: &Document) -> (Option<uuid::Uuid>, bool) {
    fn visit(
        document: &Document,
        parent: Option<Token>,
        found: &mut Vec<Document>,
        invalid: &mut bool,
    ) {
        if let Some(Node::Start(token)) = document.nodes.first() {
            if same_token(*token, search::CONVERSATION_ID) {
                if !parent.is_some_and(|parent| same_token(parent, search::AND)) {
                    *invalid = true;
                }
                found.push(document.clone());
            }
            for child in child_elements(document) {
                visit(&child, Some(*token), found, invalid);
            }
        }
    }
    let mut found = Vec::new();
    let mut invalid = false;
    visit(store, None, &mut found, &mut invalid);
    if found.len() > 1 {
        return (None, true);
    }
    let Some(node) = found.first() else {
        return (None, invalid);
    };
    let id = match node.nodes.as_slice() {
        [Node::Start(_), Node::Text(value), Node::End] => uuid::Uuid::parse_str(value).ok(),
        [Node::Start(_), Node::Opaque(value), Node::End] => <[u8; 16]>::try_from(value.as_ref())
            .ok()
            .map(uuid::Uuid::from_bytes),
        _ => None,
    };
    (id, invalid || id.is_none())
}

fn search_texts(document: &Document, include_or: bool) -> Vec<String> {
    fn visit(document: &Document, include_or: bool, output: &mut Vec<String>) {
        if let Some(Node::Start(token)) = document.nodes.first() {
            if same_token(*token, search::OR) && !include_or {
                return;
            }
            if same_token(*token, search::FREE_TEXT) {
                if let Some(value) = find_text_after(document, search::FREE_TEXT) {
                    output.push(value.to_owned());
                }
                return;
            }
            for child in child_elements(document) {
                visit(&child, include_or, output);
            }
        }
    }
    let mut output = Vec::new();
    visit(document, include_or, &mut output);
    output
}

fn search_or_element(store: &Document) -> (Option<Document>, bool) {
    fn visit(
        document: &Document,
        parent: Option<Token>,
        found: &mut Vec<Document>,
        invalid: &mut bool,
    ) {
        if let Some(Node::Start(token)) = document.nodes.first() {
            if same_token(*token, search::OR) {
                if !parent.is_some_and(|p| same_token(p, search::AND)) {
                    *invalid = true;
                }
                let children = child_elements(document);
                if children.is_empty() || children.iter().any(|child| !matches!(child.nodes.first(),
                    Some(Node::Start(t)) if [search::FREE_TEXT,search::GREATER_THAN,search::LESS_THAN]
                        .iter().any(|allowed| same_token(*t,*allowed)))) { *invalid = true; }
                found.push(document.clone());
            }
            for child in child_elements(document) {
                visit(&child, Some(*token), found, invalid);
            }
        }
    }
    let mut found = Vec::new();
    let mut invalid = false;
    visit(store, None, &mut found, &mut invalid);
    if found.len() > 1 {
        invalid = true;
    }
    (found.into_iter().next(), invalid)
}

fn search_date_comparison(
    store: &Document,
    comparison: Token,
    target_parent: Token,
) -> (Option<String>, bool) {
    fn visit(
        document: &Document,
        token: Token,
        target_parent: Token,
        parent: Option<Token>,
        found: &mut Vec<Document>,
        invalid: &mut bool,
    ) {
        if let Some(Node::Start(current)) = document.nodes.first() {
            if same_token(*current, token) {
                if parent.is_some_and(|p| same_token(p, target_parent)) {
                    found.push(document.clone());
                } else if !parent
                    .is_some_and(|p| same_token(p, search::AND) || same_token(p, search::OR))
                {
                    *invalid = true;
                }
            }
            for child in child_elements(document) {
                visit(&child, token, target_parent, Some(*current), found, invalid);
            }
        }
    }
    let mut found = Vec::new();
    let mut invalid = false;
    visit(
        store,
        comparison,
        target_parent,
        None,
        &mut found,
        &mut invalid,
    );
    if found.len() > 1 {
        return (None, true);
    }
    let Some(condition) = found.first() else {
        return (None, invalid);
    };
    let children = child_elements(condition);
    if children.len() != 2
        || !matches!(children[0].nodes.first(), Some(Node::Start(t)) if same_token(*t, email::DATE_RECEIVED))
        || !matches!(children[1].nodes.first(), Some(Node::Start(t)) if same_token(*t, search::VALUE))
    {
        return (None, true);
    }
    let value = find_text_after(&children[1], search::VALUE).map(str::to_owned);
    if value
        .as_deref()
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        .is_none()
    {
        return (None, true);
    }
    (value, invalid)
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
    pub replace_mime: bool,
    pub source_item_id: Option<String>,
    pub source_folder_id: Option<String>,
    pub source_long_id: Option<String>,
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
    let source_folder_id = find_text_after(document, compose_mail::FOLDER_ID).map(str::to_owned);
    let source_long_id = find_text_after(document, compose_mail::LONG_ID).map(str::to_owned);
    Some(ComposeMailRequest {
        mime,
        save_in_sent_items,
        replace_mime: contains_token(document, compose_mail::REPLACE_MIME),
        source_item_id,
        source_folder_id,
        source_long_id,
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

/// Parsed Ping request: heartbeat in seconds (if sent) and the watched
/// folders as (collection id, class) pairs, class being "Email",
/// "Calendar", "Contacts", ... or empty when the client omitted it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PingRequest {
    pub heartbeat: Option<u64>,
    pub folders: Vec<(String, String)>,
}

pub fn ping_request(document: &Document) -> PingRequest {
    let heartbeat = find_text_after(document, ping::LIFETIME).and_then(|v| v.trim().parse().ok());
    let mut folders: Vec<(String, String)> = Vec::new();
    let mut in_folder = false;
    let mut current: Option<&'static str> = None;
    let mut open: Vec<(u8, u8)> = Vec::new();
    for node in &document.nodes {
        match node {
            Node::Start(t) => {
                if t.has_content {
                    open.push((t.code_page, t.token));
                }
                if t.code_page == ping::PAGE {
                    if t.token == ping::FOLDER.token {
                        in_folder = true;
                        folders.push((String::new(), String::new()));
                    } else if in_folder && t.token == ping::SERVER_ENTRY_ID.token {
                        current = Some("id");
                    } else if in_folder && t.token == ping::FOLDER_TYPE.token {
                        current = Some("class");
                    }
                }
            }
            Node::Text(text) => {
                if let (Some(field), Some(last)) = (current, folders.last_mut()) {
                    match field {
                        "id" => last.0 = text.clone(),
                        _ => last.1 = text.clone(),
                    }
                }
            }
            Node::End => {
                if let Some((page, token)) = open.pop() {
                    if page == ping::PAGE {
                        if token == ping::FOLDER.token {
                            in_folder = false;
                        }
                        current = None;
                    }
                }
            }
            Node::Opaque(_) => {}
        }
    }
    folders.retain(|(id, _)| !id.is_empty());
    PingRequest { heartbeat, folders }
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
    let mut collection_start = 0;
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
                collection_start = idx;
                if token.has_content {
                    stack.push(*token);
                }
            }
            Node::Start(token) => {
                if same_token(*token, email::FLAG) && !token.has_content {
                    if let Some(command) = current_command.as_mut() {
                        command.flag_status = Some(0);
                    }
                }
                if let Some(collection) = current.as_mut() {
                    if same_token(*token, airsync::OPTIONS)
                        && stack
                            .last()
                            .is_some_and(|parent| same_token(*parent, airsync::COLLECTION))
                    {
                        collection.options_count += 1;
                    }
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
                            flag_status: None,
                            flag_fields: Vec::new(),
                            flag_valid: true,
                            unsupported_mail_change: false,
                            document: Document { nodes: Vec::new() },
                            note: NoteFields::default(),
                            contact: ContactFields::default(),
                            calendar: CalendarFields::default(),
                            task: TaskFields::default(),
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
                        } else if same_token(token, email::FLAG_STATUS)
                            && stack.iter().any(|t| same_token(*t, email::FLAG))
                        {
                            command.flag_status = Some(text.parse().unwrap_or(u8::MAX));
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
                            let size = text.parse::<usize>().unwrap_or(100);
                            collection.window_size = if size == 0 { 512 } else { size.min(512) };
                        } else if token.code_page == airsync::PAGE
                            && token.token == airsync::GET_CHANGES.token
                        {
                            collection.get_changes = text != "0";
                        } else if same_token(token, airsync::DELETES_AS_MOVES) {
                            collection.deletes_as_moves = text != "0";
                        } else if same_token(token, airsync::CLASS)
                            && stack
                                .last()
                                .is_some_and(|parent| same_token(*parent, airsync::CLASS))
                            && stack
                                .iter()
                                .any(|parent| same_token(*parent, airsync::OPTIONS))
                        {
                            collection.options_class = Some(text.clone());
                        } else if token.code_page == airsync::PAGE
                            && token.token == airsync::FILTER_TYPE.token
                        {
                            collection.filter_type = text.parse().ok();
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
                        let command_doc = Document {
                            nodes: document.nodes[start..=idx].to_vec(),
                        };
                        if command.flag_status.is_none() && empty_mail_flag(&command_doc) {
                            command.flag_status = Some(0);
                        }
                        command.flag_valid = valid_mail_flag(&command_doc, command.flag_status);
                        command.flag_fields = mail_flag_fields(&command_doc);
                        command.unsupported_mail_change = child_elements(&command_doc).iter().any(
                            |child| matches!(child.nodes.first(), Some(Node::Start(t)) if same_token(*t, email2::SEND))
                        ) || child_elements(&command_doc).iter()
                            .filter(|child| matches!(child.nodes.first(), Some(Node::Start(t)) if same_token(*t, airsync::APPLICATION_DATA)))
                            .flat_map(child_elements)
                            .any(|child| !matches!(child.nodes.first(), Some(Node::Start(t)) if same_token(*t, email::READ) || same_token(*t, email::FLAG)));
                        command.document = command_doc;
                        command.note = extract_note_fields(&document.nodes[start + 1..idx]);
                        command.contact = extract_contact_fields(&document.nodes[start + 1..idx]);
                        command.calendar = extract_calendar_fields(&document.nodes[start + 1..idx]);
                        match calendar_categories(&command.document) {
                            Ok(categories) => command.calendar.categories = categories,
                            Err(()) => command.calendar.unsupported_categories = true,
                        }
                        command.task = extract_task_fields(&document.nodes[start + 1..idx]);
                        let has_identity =
                            !command.server_id.is_empty() || !command.client_id.is_empty();
                        if has_identity {
                            collection.commands.push(command);
                        }
                    }
                }
                if ended.is_some_and(|token| same_token(token, airsync::COLLECTION)) {
                    if let Some(mut collection) = current.take() {
                        let doc = Document {
                            nodes: document.nodes[collection_start..=idx].to_vec(),
                        };
                        for options in child_elements(&doc).into_iter().filter(|child|
                            matches!(child.nodes.first(), Some(Node::Start(t)) if same_token(*t, airsync::OPTIONS))) {
                            match mime_support(&options) {
                                Ok(value) => collection.mime_support = value,
                                Err(_) => collection.body_preferences_valid = false,
                            }
                            match body_preferences(&options) {
                                Ok(preferences) => collection.body_preferences.extend(preferences),
                                Err(_) => collection.body_preferences_valid = false,
                            }
                        }
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

fn empty_mail_flag(command: &Document) -> bool {
    child_elements(command).iter()
        .filter(|child| matches!(child.nodes.first(), Some(Node::Start(t)) if same_token(*t, airsync::APPLICATION_DATA)))
        .flat_map(child_elements)
        .any(|child| matches!(child.nodes.first(), Some(Node::Start(t)) if same_token(*t, email::FLAG))
            && child_elements(&child).is_empty()
            && !child.nodes.iter().any(|node| matches!(node, Node::Text(_) | Node::Opaque(_))))
}

fn mail_flag_fields(command: &Document) -> Vec<(u8, u8, String)> {
    let Some(application_data) = child_elements(command).into_iter().find(|child| {
        matches!(child.nodes.first(), Some(Node::Start(t)) if same_token(*t, airsync::APPLICATION_DATA))
    }) else { return Vec::new() };
    let Some(flag) = child_elements(&application_data).into_iter().find(
        |child| matches!(child.nodes.first(), Some(Node::Start(t)) if same_token(*t, email::FLAG)),
    ) else {
        return Vec::new();
    };
    child_elements(&flag)
        .into_iter()
        .filter_map(|field| {
            if let Some(Node::Start(token)) = field.nodes.first() {
                if same_token(*token, email::FLAG_STATUS) {
                    return None;
                }
                return find_text_after(&field, *token)
                    .map(|value| (token.code_page, token.token, value.to_owned()));
            }
            None
        })
        .collect()
}

fn valid_mail_flag(command: &Document, status: Option<u8>) -> bool {
    let flags: Vec<_> = child_elements(command).iter()
        .filter(|child| matches!(child.nodes.first(), Some(Node::Start(t)) if same_token(*t, airsync::APPLICATION_DATA)))
        .flat_map(child_elements)
        .filter(|child| matches!(child.nodes.first(), Some(Node::Start(t)) if same_token(*t, email::FLAG)))
        .collect();
    let Some(flag) = flags.first() else {
        return status.is_none();
    };
    if flags.len() != 1 {
        return false;
    }
    let fields = child_elements(flag);
    let text = |token| {
        let mut matching = fields.iter().filter(|field| {
            matches!(field.nodes.first(),
            Some(Node::Start(start)) if same_token(*start, token))
        });
        let field = matching.next()?;
        if matching.next().is_some() {
            None
        } else {
            find_text_after(field, token)
        }
    };
    if status == Some(0) {
        let empty = fields.is_empty()
            && !flag
                .nodes
                .iter()
                .any(|node| matches!(node, Node::Text(_) | Node::Opaque(_)));
        return empty
            || text(email::FLAG_STATUS).and_then(|value| value.parse::<u8>().ok()) == Some(0);
    }
    let supported = [
        email::FLAG_STATUS,
        email::FLAG_TYPE,
        email::COMPLETE_TIME,
        tasks::DATE_COMPLETED,
        tasks::START_DATE,
        tasks::UTC_START_DATE,
        tasks::DUE_DATE,
        tasks::UTC_DUE_DATE,
        tasks::REMINDER_SET,
        tasks::REMINDER_TIME,
        tasks::ORDINAL_DATE,
        tasks::SUB_ORDINAL_DATE,
        tasks::SUBJECT,
    ];
    let mut seen = std::collections::HashSet::new();
    if fields.iter().any(|field| match field.nodes.first() {
        Some(Node::Start(token)) => {
            !supported.iter().any(|allowed| same_token(*allowed, *token))
                || !seen.insert((token.code_page, token.token))
                || find_text_after(field, *token).is_none()
        }
        _ => true,
    }) {
        return false;
    }
    let valid_date = |token| {
        text(token).is_none_or(|value| {
            value.ends_with('Z') && chrono::DateTime::parse_from_rfc3339(value).is_ok()
        })
    };
    if ![
        email::COMPLETE_TIME,
        tasks::DATE_COMPLETED,
        tasks::START_DATE,
        tasks::UTC_START_DATE,
        tasks::DUE_DATE,
        tasks::UTC_DUE_DATE,
        tasks::REMINDER_TIME,
        tasks::ORDINAL_DATE,
    ]
    .into_iter()
    .all(valid_date)
        || text(tasks::REMINDER_SET).is_some_and(|value| value != "0" && value != "1")
    {
        return false;
    }
    if status.is_none() {
        return !fields.is_empty();
    }
    if status == Some(1) {
        return text(email::FLAG_STATUS) == Some("1")
            && [tasks::DATE_COMPLETED, email::COMPLETE_TIME]
                .into_iter()
                .all(|token| {
                    text(token).is_some_and(|value| {
                        value.ends_with('Z') && chrono::DateTime::parse_from_rfc3339(value).is_ok()
                    })
                });
    }
    if status != Some(2)
        || text(email::FLAG_STATUS).and_then(|value| value.parse::<u8>().ok()) != Some(2)
        || text(email::FLAG_TYPE) != Some("Flag for follow up")
    {
        return false;
    }
    let date = |token| {
        text(token).is_some_and(|value| {
            value.ends_with('Z') && chrono::DateTime::parse_from_rfc3339(value).is_ok()
        })
    };
    let scheduled = date(tasks::START_DATE)
        && date(tasks::UTC_START_DATE)
        && date(tasks::DUE_DATE)
        && date(tasks::UTC_DUE_DATE);
    let completed = date(tasks::DATE_COMPLETED) && date(email::COMPLETE_TIME);
    scheduled || completed
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
    let mut category_buf: Option<String> = None;

    for node in nodes {
        match node {
            Node::Start(token) => {
                if same_token(*token, contacts::CATEGORIES) {
                    fields.categories.get_or_insert_with(Vec::new);
                }
                if same_token(*token, contacts::CATEGORY) {
                    category_buf = None;
                }
                if !token.has_content {
                    let empty = Some(String::new());
                    if same_token(*token, contacts::BUSINESS_ADDRESS_CITY) {
                        fields.business_address_city = empty;
                    } else if same_token(*token, contacts::BUSINESS_ADDRESS_COUNTRY) {
                        fields.business_address_country = empty;
                    } else if same_token(*token, contacts::BUSINESS_ADDRESS_POSTAL_CODE) {
                        fields.business_address_postal_code = empty;
                    } else if same_token(*token, contacts::BUSINESS_ADDRESS_STATE) {
                        fields.business_address_state = empty;
                    } else if same_token(*token, contacts::BUSINESS_ADDRESS_STREET) {
                        fields.business_address_street = empty;
                    } else if same_token(*token, contacts::HOME_ADDRESS_CITY) {
                        fields.home_address_city = empty;
                    } else if same_token(*token, contacts::HOME_ADDRESS_COUNTRY) {
                        fields.home_address_country = empty;
                    } else if same_token(*token, contacts::HOME_ADDRESS_POSTAL_CODE) {
                        fields.home_address_postal_code = empty;
                    } else if same_token(*token, contacts::HOME_ADDRESS_STATE) {
                        fields.home_address_state = empty;
                    } else if same_token(*token, contacts::HOME_ADDRESS_STREET) {
                        fields.home_address_street = empty;
                    } else if same_token(*token, contacts::OTHER_ADDRESS_CITY) {
                        fields.other_address_city = empty;
                    } else if same_token(*token, contacts::OTHER_ADDRESS_COUNTRY) {
                        fields.other_address_country = empty;
                    } else if same_token(*token, contacts::OTHER_ADDRESS_POSTAL_CODE) {
                        fields.other_address_postal_code = empty;
                    } else if same_token(*token, contacts::OTHER_ADDRESS_STATE) {
                        fields.other_address_state = empty;
                    } else if same_token(*token, contacts::OTHER_ADDRESS_STREET) {
                        fields.other_address_street = empty;
                    } else if same_token(*token, contacts::MIDDLE_NAME) {
                        fields.middle_name = empty;
                    } else if same_token(*token, contacts::SUFFIX) {
                        fields.suffix = empty;
                    } else if same_token(*token, contacts::BIRTHDAY) {
                        fields.birthday = empty;
                    } else if same_token(*token, contacts::ANNIVERSARY) {
                        fields.anniversary = empty;
                    } else if same_token(*token, contacts::TITLE) {
                        fields.name_prefix = empty;
                    } else if same_token(*token, contacts::YOMI_FIRST_NAME) {
                        fields.yomi_first_name = empty;
                    } else if same_token(*token, contacts::YOMI_LAST_NAME) {
                        fields.yomi_last_name = empty;
                    } else if same_token(*token, contacts::YOMI_COMPANY_NAME) {
                        fields.yomi_company_name = empty;
                    } else if same_token(*token, contacts::OFFICE_LOCATION) {
                        fields.office_location = empty;
                    } else if same_token(*token, contacts2::NICK_NAME) {
                        fields.nickname = empty;
                    } else if same_token(*token, contacts2::COMPANY_MAIN_PHONE) {
                        fields.company_main_phone = empty;
                    } else if same_token(*token, contacts2::MMS) {
                        fields.mms = empty;
                    } else if same_token(*token, contacts2::ACCOUNT_NAME) {
                        fields.account_name = empty;
                    } else if same_token(*token, contacts::PICTURE) {
                        fields.picture = empty;
                    } else if same_token(*token, contacts2::IM_ADDRESS) {
                        fields.im_address = empty;
                    } else if same_token(*token, contacts2::IM_ADDRESS_2) {
                        fields.im_address_2 = empty;
                    } else if same_token(*token, contacts2::IM_ADDRESS_3) {
                        fields.im_address_3 = empty;
                    } else if same_token(*token, contacts::DEPARTMENT) {
                        fields.department = empty;
                    } else if same_token(*token, contacts::BUSINESS_2_PHONE_NUMBER) {
                        fields.business_2_phone_number = empty;
                    } else if same_token(*token, contacts::HOME_2_PHONE_NUMBER) {
                        fields.home_2_phone_number = empty;
                    } else if same_token(*token, contacts::HOME_FAX_NUMBER) {
                        fields.home_fax_number = empty;
                    } else if same_token(*token, contacts::BUSINESS_FAX_NUMBER) {
                        fields.business_fax_number = empty;
                    } else if same_token(*token, contacts::PAGER_NUMBER) {
                        fields.pager_number = empty;
                    } else if same_token(*token, contacts::CAR_PHONE_NUMBER) {
                        fields.car_phone_number = empty;
                    } else if same_token(*token, contacts::RADIO_PHONE_NUMBER) {
                        fields.radio_phone_number = empty;
                    } else if same_token(*token, contacts::WEB_PAGE) {
                        fields.web_page = empty;
                    } else if same_token(*token, contacts::ASSISTANT_PHONE_NUMBER) {
                        fields.assistant_phone_number = empty;
                    }
                }
                path.push(*token);
            }
            Node::Text(text) => {
                let Some(&top) = path.last() else { continue };
                if same_token(top, contacts::FIRST_NAME) {
                    fields.first_name = Some(text.clone());
                } else if same_token(top, contacts::LAST_NAME) {
                    fields.last_name = Some(text.clone());
                } else if same_token(top, contacts::MIDDLE_NAME) {
                    fields.middle_name = Some(text.clone());
                } else if same_token(top, contacts::SUFFIX) {
                    fields.suffix = Some(text.clone());
                } else if same_token(top, contacts::BIRTHDAY) {
                    fields.birthday = Some(text.clone());
                } else if same_token(top, contacts::ANNIVERSARY) {
                    fields.anniversary = Some(text.clone());
                } else if same_token(top, contacts::TITLE) {
                    fields.name_prefix = Some(text.clone());
                } else if same_token(top, contacts::YOMI_FIRST_NAME) {
                    fields.yomi_first_name = Some(text.clone());
                } else if same_token(top, contacts::YOMI_LAST_NAME) {
                    fields.yomi_last_name = Some(text.clone());
                } else if same_token(top, contacts::YOMI_COMPANY_NAME) {
                    fields.yomi_company_name = Some(text.clone());
                } else if same_token(top, contacts::OFFICE_LOCATION) {
                    fields.office_location = Some(text.clone());
                } else if same_token(top, contacts2::NICK_NAME) {
                    fields.nickname = Some(text.clone());
                } else if same_token(top, contacts2::COMPANY_MAIN_PHONE) {
                    fields.company_main_phone = Some(text.clone());
                } else if same_token(top, contacts2::MMS) {
                    fields.mms = Some(text.clone());
                } else if same_token(top, contacts2::ACCOUNT_NAME) {
                    fields.account_name = Some(text.clone());
                } else if same_token(top, contacts::PICTURE) {
                    fields.picture = Some(text.clone());
                } else if same_token(top, contacts2::IM_ADDRESS) {
                    fields.im_address = Some(text.clone());
                } else if same_token(top, contacts2::IM_ADDRESS_2) {
                    fields.im_address_2 = Some(text.clone());
                } else if same_token(top, contacts2::IM_ADDRESS_3) {
                    fields.im_address_3 = Some(text.clone());
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
                } else if same_token(top, contacts::BUSINESS_2_PHONE_NUMBER) {
                    fields.business_2_phone_number = Some(text.clone());
                } else if same_token(top, contacts::HOME_2_PHONE_NUMBER) {
                    fields.home_2_phone_number = Some(text.clone());
                } else if same_token(top, contacts::HOME_FAX_NUMBER) {
                    fields.home_fax_number = Some(text.clone());
                } else if same_token(top, contacts::BUSINESS_FAX_NUMBER) {
                    fields.business_fax_number = Some(text.clone());
                } else if same_token(top, contacts::PAGER_NUMBER) {
                    fields.pager_number = Some(text.clone());
                } else if same_token(top, contacts::CAR_PHONE_NUMBER) {
                    fields.car_phone_number = Some(text.clone());
                } else if same_token(top, contacts::RADIO_PHONE_NUMBER) {
                    fields.radio_phone_number = Some(text.clone());
                } else if same_token(top, contacts::WEB_PAGE) {
                    fields.web_page = Some(text.clone());
                } else if same_token(top, contacts::ASSISTANT_PHONE_NUMBER) {
                    fields.assistant_phone_number = Some(text.clone());
                } else if same_token(top, contacts::COMPANY_NAME) {
                    fields.company_name = Some(text.clone());
                } else if same_token(top, contacts::DEPARTMENT) {
                    fields.department = Some(text.clone());
                } else if same_token(top, contacts::JOB_TITLE) {
                    fields.job_title = Some(text.clone());
                } else if same_token(top, contacts::BUSINESS_ADDRESS_CITY) {
                    fields.business_address_city = Some(text.clone());
                } else if same_token(top, contacts::BUSINESS_ADDRESS_COUNTRY) {
                    fields.business_address_country = Some(text.clone());
                } else if same_token(top, contacts::BUSINESS_ADDRESS_POSTAL_CODE) {
                    fields.business_address_postal_code = Some(text.clone());
                } else if same_token(top, contacts::BUSINESS_ADDRESS_STATE) {
                    fields.business_address_state = Some(text.clone());
                } else if same_token(top, contacts::BUSINESS_ADDRESS_STREET) {
                    fields.business_address_street = Some(text.clone());
                } else if same_token(top, contacts::HOME_ADDRESS_CITY) {
                    fields.home_address_city = Some(text.clone());
                } else if same_token(top, contacts::HOME_ADDRESS_COUNTRY) {
                    fields.home_address_country = Some(text.clone());
                } else if same_token(top, contacts::HOME_ADDRESS_POSTAL_CODE) {
                    fields.home_address_postal_code = Some(text.clone());
                } else if same_token(top, contacts::HOME_ADDRESS_STATE) {
                    fields.home_address_state = Some(text.clone());
                } else if same_token(top, contacts::HOME_ADDRESS_STREET) {
                    fields.home_address_street = Some(text.clone());
                } else if same_token(top, contacts::OTHER_ADDRESS_CITY) {
                    fields.other_address_city = Some(text.clone());
                } else if same_token(top, contacts::OTHER_ADDRESS_COUNTRY) {
                    fields.other_address_country = Some(text.clone());
                } else if same_token(top, contacts::OTHER_ADDRESS_POSTAL_CODE) {
                    fields.other_address_postal_code = Some(text.clone());
                } else if same_token(top, contacts::OTHER_ADDRESS_STATE) {
                    fields.other_address_state = Some(text.clone());
                } else if same_token(top, contacts::OTHER_ADDRESS_STREET) {
                    fields.other_address_street = Some(text.clone());
                } else if same_token(top, contacts::CATEGORY)
                    && path.len() >= 2
                    && same_token(path[path.len() - 2], contacts::CATEGORIES)
                {
                    category_buf = Some(text.clone());
                }
            }
            Node::End => {
                if let Some(top) = path.pop() {
                    if same_token(top, contacts::CATEGORY) {
                        if let (Some(categories), Some(category)) =
                            (&mut fields.categories, category_buf.take())
                        {
                            categories.push(category);
                        }
                    }
                }
            }
            Node::Opaque(_) => {}
        }
    }

    fields
}

fn calendar_categories(command: &Document) -> Result<Option<Vec<String>>, ()> {
    let applications: Vec<_> = child_elements(command).into_iter()
        .filter(|child| matches!(child.nodes.first(), Some(Node::Start(token)) if same_token(*token, airsync::APPLICATION_DATA)))
        .collect();
    let categories: Vec<_> = applications.iter().flat_map(child_elements)
        .filter(|child| matches!(child.nodes.first(), Some(Node::Start(token)) if same_token(*token, calendar::CATEGORIES)))
        .collect();
    if categories.len() > 1 {
        return Err(());
    }
    let Some(container) = categories.first() else {
        return Ok(None);
    };
    let mut depth = 0usize;
    for node in &container.nodes {
        match node {
            Node::Start(token) if token.has_content => depth += 1,
            Node::End => depth = depth.checked_sub(1).ok_or(())?,
            Node::Text(_) | Node::Opaque(_) if depth == 1 => return Err(()),
            _ => {}
        }
    }
    let mut values = std::collections::BTreeSet::new();
    for child in child_elements(container) {
        if !matches!(child.nodes.first(), Some(Node::Start(token)) if same_token(*token, calendar::CATEGORY) && token.has_content)
        {
            return Err(());
        }
        let mut value = String::new();
        for node in child
            .nodes
            .iter()
            .skip(1)
            .take(child.nodes.len().saturating_sub(2))
        {
            if let Node::Text(text) = node {
                value.push_str(text);
            } else {
                return Err(());
            }
        }
        if value.is_empty() || !values.insert(value) {
            return Err(());
        }
    }
    Ok(Some(values.into_iter().collect()))
}

/// Mostly flat leaves like `extract_contact_fields`, except Attendees --
/// `calendar:Attendees` (0x07) contains repeated `calendar:Attendee`
/// (0x08) children, each with its own Email/Name/Type leaves, so those
/// need a small accumulator (`current_attendee`) rather than the
/// path-top check the flat fields use.
fn extract_calendar_fields(nodes: &[Node]) -> CalendarFields {
    let mut fields = CalendarFields::default();
    let mut path: Vec<Token> = Vec::new();
    let mut current_attendee: Option<AttendeeField> = None;
    let mut body_count = 0;
    let mut type_count = 0;
    let mut data_count = 0;
    let mut attendees_count = 0;
    let mut busy_count = 0;
    let mut busy_text = String::new();
    let mut sensitivity_count = 0;
    let mut sensitivity_text = String::new();
    let mut reminder_count = 0;
    let mut reminder_text = String::new();
    let mut latitude: Option<String> = None;
    let mut longitude: Option<String> = None;

    for node in nodes {
        match node {
            Node::Start(token) => {
                if same_token(*token, calendar::RESPONSE_TYPE)
                    && path
                        .last()
                        .is_some_and(|parent| same_token(*parent, airsync::APPLICATION_DATA))
                {
                    fields.unsupported_response_type = true;
                }
                if (same_token(*token, calendar::RECURRENCE)
                    || same_token(*token, calendar::EXCEPTIONS))
                    && path
                        .last()
                        .is_some_and(|parent| same_token(*parent, airsync::APPLICATION_DATA))
                {
                    fields.unsupported_recurrence = true;
                }
                if same_token(*token, calendar::REMINDER)
                    && path
                        .last()
                        .is_some_and(|parent| same_token(*parent, airsync::APPLICATION_DATA))
                {
                    reminder_count += 1;
                    reminder_text.clear();
                    fields.reminder = Some(None);
                }
                if path
                    .last()
                    .is_some_and(|parent| same_token(*parent, calendar::REMINDER))
                {
                    fields.unsupported_reminder = true;
                }
                if same_token(*token, calendar::BUSY_STATUS)
                    && path
                        .last()
                        .is_some_and(|parent| same_token(*parent, airsync::APPLICATION_DATA))
                {
                    busy_count += 1;
                    busy_text.clear();
                    if !token.has_content {
                        fields.unsupported_busy_status = true;
                    }
                }
                if same_token(*token, calendar::SENSITIVITY)
                    && path
                        .last()
                        .is_some_and(|parent| same_token(*parent, airsync::APPLICATION_DATA))
                {
                    sensitivity_count += 1;
                    sensitivity_text.clear();
                    if !token.has_content {
                        fields.unsupported_sensitivity = true;
                    }
                }
                if path
                    .last()
                    .is_some_and(|parent| same_token(*parent, calendar::SENSITIVITY))
                {
                    fields.unsupported_sensitivity = true;
                }
                if same_token(*token, calendar::SUBJECT) {
                    fields.subject = Some(String::new());
                }
                if same_token(*token, airsync_base::BODY)
                    && path
                        .last()
                        .is_some_and(|t| same_token(*t, airsync::APPLICATION_DATA))
                {
                    body_count += 1;
                    fields.body = Some(String::new());
                    fields.body_type = Some(1);
                } else if path.windows(2).any(|pair| {
                    same_token(pair[0], airsync::APPLICATION_DATA)
                        && same_token(pair[1], airsync_base::BODY)
                }) && path
                    .last()
                    .is_some_and(|t| same_token(*t, airsync_base::BODY))
                {
                    if same_token(*token, airsync_base::DATA) {
                        data_count += 1;
                        fields.body = Some(String::new());
                    } else if same_token(*token, airsync_base::TYPE) {
                        type_count += 1;
                        fields.body_type = None;
                    }
                }
                if same_token(*token, calendar::ATTENDEES)
                    && path
                        .last()
                        .is_some_and(|t| same_token(*t, airsync::APPLICATION_DATA))
                {
                    attendees_count += 1;
                    fields.attendees_present = true;
                }
                if same_token(*token, calendar::ATTENDEE)
                    && path.windows(2).any(|pair| {
                        same_token(pair[0], airsync::APPLICATION_DATA)
                            && same_token(pair[1], calendar::ATTENDEES)
                    })
                    && path
                        .last()
                        .is_some_and(|t| same_token(*t, calendar::ATTENDEES))
                {
                    if !token.has_content {
                        fields.unsupported_attendees = true;
                    } else {
                        current_attendee = Some(AttendeeField::default());
                    }
                }
                if path
                    .last()
                    .is_some_and(|t| same_token(*t, airsync_base::LOCATION))
                    && !same_token(*token, airsync_base::DISPLAY_NAME)
                {
                    let coordinate = if same_token(*token, airsync_base::LATITUDE) {
                        Some(&mut latitude)
                    } else if same_token(*token, airsync_base::LONGITUDE) {
                        Some(&mut longitude)
                    } else {
                        None
                    };
                    if let Some(coordinate) = coordinate {
                        if coordinate.is_some() || !token.has_content {
                            fields.unsupported_location = true;
                        }
                        *coordinate = Some(String::new());
                    } else {
                        fields.unsupported_location = true;
                    }
                }
                if same_token(*token, calendar::LOCATION)
                    || same_token(*token, airsync_base::LOCATION)
                    || (same_token(*token, airsync_base::DISPLAY_NAME)
                        && path
                            .last()
                            .is_some_and(|t| same_token(*t, airsync_base::LOCATION)))
                {
                    fields.location = Some(String::new());
                }
                if token.has_content {
                    path.push(*token);
                }
            }
            Node::Text(text) => {
                let Some(&top) = path.last() else { continue };
                if let Some(attendee) = current_attendee.as_mut() {
                    if same_token(top, calendar::ATTENDEE_EMAIL) {
                        attendee.email = text.clone();
                    } else if same_token(top, calendar::ATTENDEE_NAME) {
                        attendee.name = Some(text.clone());
                    } else if same_token(top, calendar::ATTENDEE_TYPE) {
                        attendee.optional = text == "2";
                    }
                    continue;
                }
                if path.windows(2).any(|pair| {
                    same_token(pair[0], airsync::APPLICATION_DATA)
                        && same_token(pair[1], airsync_base::BODY)
                }) {
                    if same_token(top, airsync_base::DATA) {
                        fields.body = Some(text.clone());
                    } else if same_token(top, airsync_base::TYPE) {
                        fields.body_type = text.parse::<u8>().ok();
                        if !matches!(fields.body_type, Some(1 | 2)) {
                            fields.unsupported_body = true;
                        }
                    } else if same_token(top, airsync_base::TRUNCATED) && text != "0" {
                        fields.unsupported_body = true;
                    }
                } else if same_token(top, calendar::SUBJECT) {
                    fields.subject = Some(text.clone());
                } else if same_token(top, calendar::LOCATION)
                    || (same_token(top, airsync_base::DISPLAY_NAME)
                        && path.iter().any(|t| same_token(*t, airsync_base::LOCATION)))
                {
                    fields.location = Some(text.clone());
                } else if same_token(top, airsync_base::LATITUDE)
                    && path.iter().any(|t| same_token(*t, airsync_base::LOCATION))
                {
                    if let Some(value) = &mut latitude {
                        value.push_str(text);
                    }
                } else if same_token(top, airsync_base::LONGITUDE)
                    && path.iter().any(|t| same_token(*t, airsync_base::LOCATION))
                {
                    if let Some(value) = &mut longitude {
                        value.push_str(text);
                    }
                } else if same_token(top, calendar::START_TIME) {
                    fields.start_time = Some(text.clone());
                } else if same_token(top, calendar::END_TIME) {
                    fields.end_time = Some(text.clone());
                } else if same_token(top, calendar::REMINDER)
                    && path.len() >= 2
                    && same_token(path[path.len() - 2], airsync::APPLICATION_DATA)
                {
                    reminder_text.push_str(text);
                } else if same_token(top, calendar::BUSY_STATUS)
                    && path.len() >= 2
                    && same_token(path[path.len() - 2], airsync::APPLICATION_DATA)
                {
                    busy_text.push_str(text);
                } else if same_token(top, calendar::SENSITIVITY)
                    && path.len() >= 2
                    && same_token(path[path.len() - 2], airsync::APPLICATION_DATA)
                {
                    sensitivity_text.push_str(text);
                } else if same_token(top, calendar::ALL_DAY_EVENT) {
                    fields.all_day_event = Some(text != "0");
                }
            }
            Node::End => {
                if path.len() >= 2
                    && same_token(path[path.len() - 1], calendar::BUSY_STATUS)
                    && same_token(path[path.len() - 2], airsync::APPLICATION_DATA)
                {
                    fields.busy_status = busy_text.parse::<u8>().ok();
                    if !matches!(fields.busy_status, Some(0..=3)) {
                        fields.unsupported_busy_status = true;
                    }
                }
                if path.len() >= 2
                    && same_token(path[path.len() - 1], calendar::SENSITIVITY)
                    && same_token(path[path.len() - 2], airsync::APPLICATION_DATA)
                {
                    fields.sensitivity = sensitivity_text.parse::<u8>().ok();
                    if !matches!(fields.sensitivity, Some(0 | 2)) {
                        fields.unsupported_sensitivity = true;
                    }
                }
                if path.len() >= 2
                    && same_token(path[path.len() - 1], calendar::REMINDER)
                    && same_token(path[path.len() - 2], airsync::APPLICATION_DATA)
                    && !reminder_text.is_empty()
                {
                    match reminder_text.parse::<u32>() {
                        Ok(minutes) => fields.reminder = Some(Some(minutes)),
                        Err(_) => fields.unsupported_reminder = true,
                    }
                }
                if path
                    .last()
                    .is_some_and(|&t| same_token(t, calendar::ATTENDEE))
                {
                    if let Some(attendee) = current_attendee.take() {
                        if !attendee.email.is_empty() {
                            fields.attendees.push(attendee);
                        } else {
                            fields.unsupported_attendees = true;
                        }
                    }
                }
                path.pop();
            }
            Node::Opaque(_) => {
                if path
                    .last()
                    .is_some_and(|token| same_token(*token, calendar::SENSITIVITY))
                {
                    fields.unsupported_sensitivity = true;
                }
                if path
                    .last()
                    .is_some_and(|token| same_token(*token, calendar::REMINDER))
                {
                    fields.unsupported_reminder = true;
                }
                if path.iter().any(|t| same_token(*t, airsync_base::BODY)) {
                    fields.unsupported_body = true;
                }
            }
        }
    }

    match (latitude, longitude) {
        (None, None) => {}
        (Some(latitude), Some(longitude)) => {
            match (latitude.parse::<f64>(), longitude.parse::<f64>()) {
                (Ok(lat), Ok(lon))
                    if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) =>
                {
                    fields.location_coordinates = Some((lat.to_string(), lon.to_string()));
                }
                _ => fields.unsupported_location = true,
            }
        }
        _ => fields.unsupported_location = true,
    }
    fields.unsupported_sensitivity |=
        sensitivity_count > 1 || (sensitivity_count == 1 && fields.sensitivity.is_none());
    fields.unsupported_reminder |= reminder_count > 1;
    fields.unsupported_busy_status |=
        busy_count > 1 || (busy_count == 1 && fields.busy_status.is_none());
    let mut attendee_emails = std::collections::HashSet::new();
    fields.unsupported_attendees |= attendees_count > 1
        || fields
            .attendees
            .iter()
            .any(|attendee| !attendee_emails.insert(attendee.email.to_lowercase()));
    fields.unsupported_body |= body_count > 1
        || type_count > 1
        || data_count > 1
        || (type_count > 0 && fields.body_type.is_none());
    fields
}

/// Same shape as `extract_calendar_fields` -- flat leaves, no nesting for
/// the fields this gateway round-trips.
fn extract_task_fields(nodes: &[Node]) -> TaskFields {
    let body = extract_calendar_fields(nodes);
    let mut fields = TaskFields {
        body: body.body,
        body_type: body.body_type,
        unsupported_body: body.unsupported_body,
        ..Default::default()
    };
    let mut path: Vec<Token> = Vec::new();

    for node in nodes {
        match node {
            Node::Start(token) => path.push(*token),
            Node::Text(text) => {
                let Some(&top) = path.last() else { continue };
                if same_token(top, tasks::SUBJECT) {
                    fields.subject = Some(text.clone());
                } else if same_token(top, tasks::DUE_DATE) {
                    fields.due_date = Some(text.clone());
                } else if same_token(top, tasks::UTC_DUE_DATE) {
                    fields.utc_due_date = Some(text.clone());
                } else if same_token(top, tasks::COMPLETE) {
                    fields.complete = Some(text != "0");
                } else if same_token(top, tasks::DATE_COMPLETED) {
                    fields.date_completed = Some(text.clone());
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

    /// Current position, for capturing what is written after it.
    pub fn mark(&self) -> usize {
        self.nodes.len()
    }

    /// Copy of everything written since `mark`.
    pub fn nodes_since(&self, mark: usize) -> Vec<Node> {
        self.nodes[mark.min(self.nodes.len())..].to_vec()
    }

    /// Append previously captured nodes verbatim.
    pub fn extend(&mut self, nodes: Vec<Node>) {
        self.nodes.extend(nodes);
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
        builder.leaf(contacts::MIDDLE_NAME, "Augusta");
        builder.leaf(contacts::LAST_NAME, "Lovelace");
        builder.leaf(contacts::SUFFIX, "Jr.");
        builder.leaf(contacts::TITLE, "Countess");
        builder.leaf(contacts::YOMI_FIRST_NAME, "エイダ");
        builder.leaf(contacts::YOMI_LAST_NAME, "ラブレス");
        builder.leaf(contacts::YOMI_COMPANY_NAME, "アナリティカル エンジンズ");
        builder.leaf(contacts::OFFICE_LOCATION, "Building 4, Floor 2");
        builder.leaf(contacts2::NICK_NAME, "Enchantress of Numbers");
        builder.leaf(contacts2::COMPANY_MAIN_PHONE, "555-0199");
        builder.leaf(contacts2::MMS, "ada@mms.example");
        builder.leaf(contacts2::ACCOUNT_NAME, "ada-eas-account");
        builder.leaf(contacts::PICTURE, "AQIDBA==");
        builder.leaf(contacts2::IM_ADDRESS, "ada@example.im");
        builder.leaf(contacts2::IM_ADDRESS_2, "ada@chat.example");
        builder.leaf(contacts2::IM_ADDRESS_3, "ada@xmpp.example");
        builder.leaf(contacts::BIRTHDAY, "18151210");
        builder.leaf(contacts::ANNIVERSARY, "18351108");
        builder.leaf(contacts::EMAIL1_ADDRESS, "ada@example.com");
        builder.leaf(contacts::MOBILE_PHONE_NUMBER, "555-0100");
        builder.leaf(contacts::HOME_2_PHONE_NUMBER, "555-0102");
        builder.leaf(contacts::BUSINESS_2_PHONE_NUMBER, "555-0103");
        builder.leaf(contacts::HOME_FAX_NUMBER, "555-0105");
        builder.leaf(contacts::BUSINESS_FAX_NUMBER, "555-0109");
        builder.leaf(contacts::PAGER_NUMBER, "555-0106");
        builder.leaf(contacts::CAR_PHONE_NUMBER, "555-0107");
        builder.leaf(contacts::RADIO_PHONE_NUMBER, "555-0108");
        builder.leaf(contacts::ASSISTANT_PHONE_NUMBER, "555-0110");
        builder.leaf(contacts::WEB_PAGE, "https://example.com/ada");
        builder.leaf(contacts::COMPANY_NAME, "Analytical Engines Ltd");
        builder.leaf(contacts::DEPARTMENT, "Research");
        builder.leaf(contacts::BUSINESS_ADDRESS_CITY, "Toronto");
        builder.leaf(contacts::BUSINESS_ADDRESS_COUNTRY, "Canada");
        builder.leaf(contacts::BUSINESS_ADDRESS_POSTAL_CODE, "M5V 2T6");
        builder.leaf(contacts::BUSINESS_ADDRESS_STATE, "Ontario");
        builder.leaf(contacts::BUSINESS_ADDRESS_STREET, "100 King St W");
        builder.leaf(contacts::HOME_ADDRESS_CITY, "Ottawa");
        builder.empty_tag(contacts::OTHER_ADDRESS_CITY);
        builder.start(contacts::CATEGORIES);
        builder.leaf(contacts::CATEGORY, "Work");
        builder.leaf(contacts::CATEGORY, "Friends");
        builder.end();
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
        assert_eq!(command.contact.middle_name.as_deref(), Some("Augusta"));
        assert_eq!(command.contact.last_name.as_deref(), Some("Lovelace"));
        assert_eq!(command.contact.suffix.as_deref(), Some("Jr."));
        assert_eq!(command.contact.name_prefix.as_deref(), Some("Countess"));
        assert_eq!(command.contact.yomi_first_name.as_deref(), Some("エイダ"));
        assert_eq!(command.contact.yomi_last_name.as_deref(), Some("ラブレス"));
        assert_eq!(
            command.contact.yomi_company_name.as_deref(),
            Some("アナリティカル エンジンズ")
        );
        assert_eq!(
            command.contact.office_location.as_deref(),
            Some("Building 4, Floor 2")
        );
        assert_eq!(
            command.contact.nickname.as_deref(),
            Some("Enchantress of Numbers")
        );
        assert_eq!(command.contact.birthday.as_deref(), Some("18151210"));
        assert_eq!(command.contact.anniversary.as_deref(), Some("18351108"));
        assert_eq!(
            command.contact.email1_address.as_deref(),
            Some("ada@example.com")
        );
        assert_eq!(
            command.contact.mobile_phone_number.as_deref(),
            Some("555-0100")
        );
        assert_eq!(
            command.contact.home_2_phone_number.as_deref(),
            Some("555-0102")
        );
        assert_eq!(
            command.contact.business_2_phone_number.as_deref(),
            Some("555-0103")
        );
        assert_eq!(command.contact.home_fax_number.as_deref(), Some("555-0105"));
        assert_eq!(
            command.contact.business_fax_number.as_deref(),
            Some("555-0109")
        );
        assert_eq!(command.contact.pager_number.as_deref(), Some("555-0106"));
        assert_eq!(
            command.contact.car_phone_number.as_deref(),
            Some("555-0107")
        );
        assert_eq!(
            command.contact.radio_phone_number.as_deref(),
            Some("555-0108")
        );
        assert_eq!(
            command.contact.assistant_phone_number.as_deref(),
            Some("555-0110")
        );
        assert_eq!(
            command.contact.web_page.as_deref(),
            Some("https://example.com/ada")
        );
        assert_eq!(
            command.contact.company_name.as_deref(),
            Some("Analytical Engines Ltd")
        );
        assert_eq!(command.contact.department.as_deref(), Some("Research"));
        assert_eq!(command.contact.email2_address, None);
        assert_eq!(
            command.contact.business_address_city.as_deref(),
            Some("Toronto")
        );
        assert_eq!(
            command.contact.business_address_country.as_deref(),
            Some("Canada")
        );
        assert_eq!(
            command.contact.business_address_postal_code.as_deref(),
            Some("M5V 2T6")
        );
        assert_eq!(
            command.contact.business_address_state.as_deref(),
            Some("Ontario")
        );
        assert_eq!(
            command.contact.business_address_street.as_deref(),
            Some("100 King St W")
        );
        assert_eq!(command.contact.home_address_city.as_deref(), Some("Ottawa"));
        assert_eq!(command.contact.other_address_city.as_deref(), Some(""));
        assert_eq!(
            command.contact.categories.as_deref(),
            Some(&["Work".to_owned(), "Friends".to_owned()][..])
        );
    }

    #[test]
    fn parses_task_add_with_subject_due_date_and_complete() {
        // Same shape as parses_contact_change_with_flat_fields, but an Add
        // (client_id, no server_id yet) against a `task_`-prefixed
        // collection, exercising extract_task_fields().
        let mut builder = DocumentBuilder::new();
        builder.start(airsync::SYNC);
        builder.start(airsync::COLLECTIONS);
        builder.start(airsync::COLLECTION);
        builder.leaf(airsync::SYNC_KEY, "1");
        builder.leaf(airsync::COLLECTION_ID, "task_x");
        builder.start(airsync::COMMANDS);
        builder.start(airsync::ADD);
        builder.leaf(airsync::CLIENT_ID, "client-task-1");
        builder.start(airsync::APPLICATION_DATA);
        builder.leaf(tasks::SUBJECT, "Renew passport");
        builder.leaf(tasks::COMPLETE, "0");
        builder.leaf(tasks::DUE_DATE, "20260905T090000Z");
        builder.leaf(tasks::DATE_COMPLETED, "2026-09-04T15:30:00.000Z");
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
        assert_eq!(command.client_id, "client-task-1");
        assert_eq!(command.task.subject.as_deref(), Some("Renew passport"));
        assert_eq!(command.task.complete, Some(false));
        assert_eq!(command.task.due_date.as_deref(), Some("20260905T090000Z"));
        assert_eq!(
            command.task.date_completed.as_deref(),
            Some("2026-09-04T15:30:00.000Z")
        );
    }

    #[test]
    fn calendar_numeric_fields_join_fragmented_text() {
        let mut b = DocumentBuilder::new();
        b.start(airsync::APPLICATION_DATA);
        b.start(calendar::REMINDER);
        b.extend(vec![Node::Text("1".to_owned()), Node::Text("5".to_owned())]);
        b.end();
        b.start(calendar::BUSY_STATUS);
        b.extend(vec![Node::Text("1".to_owned()), Node::Text("2".to_owned())]);
        b.end();
        b.end();
        let fields = super::extract_calendar_fields(&b.finish().nodes);
        assert_eq!(fields.reminder, Some(Some(15)));
        assert!(fields.unsupported_busy_status);
    }

    #[test]
    fn calendar_reminder_distinguishes_absent_clear_zero_and_invalid() {
        let parse = |values: &[Option<&str>]| {
            let mut b = DocumentBuilder::new();
            b.start(airsync::APPLICATION_DATA);
            for value in values {
                if let Some(value) = value {
                    b.leaf(calendar::REMINDER, *value);
                } else {
                    b.empty_tag(calendar::REMINDER);
                }
            }
            b.end();
            super::extract_calendar_fields(&b.finish().nodes)
        };
        assert_eq!(parse(&[]).reminder, None);
        assert_eq!(parse(&[None]).reminder, Some(None));
        assert_eq!(parse(&[Some("0")]).reminder, Some(Some(0)));
        assert_eq!(parse(&[Some("4294967295")]).reminder, Some(Some(u32::MAX)));
        assert!(parse(&[Some("4294967296")]).unsupported_reminder);
        assert!(parse(&[Some("-1")]).unsupported_reminder);
        assert!(parse(&[Some("1"), Some("2")]).unsupported_reminder);
    }

    #[test]
    fn search_conversation_requires_one_guid_inside_and() {
        fn request(parent: super::Token, values: &[&str]) -> super::Document {
            let mut b = super::DocumentBuilder::new();
            b.start(super::search::SEARCH);
            b.start(super::search::STORE);
            b.leaf(super::search::NAME, "Mailbox");
            b.start(super::search::QUERY);
            b.start(parent);
            for value in values {
                b.leaf(super::search::CONVERSATION_ID, *value);
            }
            b.end();
            b.end();
            b.end();
            b.end();
            b.finish()
        }
        let guid = "00112233-4455-6677-8899-aabbccddeeff";
        let parsed = super::search_request(&request(super::search::AND, &[guid])).unwrap();
        assert!(!parsed.invalid_conversation);
        assert!(!parsed.unsupported_query);
        assert_eq!(
            parsed.conversation_id,
            Some(uuid::Uuid::parse_str(guid).unwrap())
        );
        assert!(
            super::search_request(&request(super::search::OR, &[guid]))
                .unwrap()
                .invalid_conversation
        );
        assert!(
            super::search_request(&request(super::search::AND, &[guid, guid]))
                .unwrap()
                .invalid_conversation
        );
        assert!(
            super::search_request(&request(super::search::AND, &["not-a-guid"]))
                .unwrap()
                .invalid_conversation
        );
    }

    #[test]
    fn calendar_sensitivity_only_accepts_supported_values() {
        fn parse(values: &[&str]) -> super::CalendarFields {
            let mut b = super::DocumentBuilder::new();
            b.start(super::airsync::APPLICATION_DATA);
            for value in values {
                b.leaf(super::calendar::SENSITIVITY, *value);
            }
            b.end();
            super::extract_calendar_fields(&b.finish().nodes)
        }
        for (text, expected) in [("0", 0), ("2", 2)] {
            let fields = parse(&[text]);
            assert_eq!(fields.sensitivity, Some(expected));
            assert!(!fields.unsupported_sensitivity);
        }
        assert!(!parse(&[]).unsupported_sensitivity);
        for values in [
            &["1"][..],
            &["3"][..],
            &["4"][..],
            &["bad"][..],
            &["0", "2"][..],
            &[""][..],
        ] {
            assert!(parse(values).unsupported_sensitivity);
        }
    }

    #[test]
    fn calendar_response_type_is_rejected_in_client_changes() {
        let mut b = super::DocumentBuilder::new();
        b.start(super::airsync::APPLICATION_DATA);
        b.leaf(super::calendar::RESPONSE_TYPE, "3");
        b.end();
        assert!(super::extract_calendar_fields(&b.finish().nodes).unsupported_response_type);
    }

    #[test]
    fn calendar_recurrence_and_exceptions_are_not_silently_accepted() {
        for token in [super::calendar::RECURRENCE, super::calendar::EXCEPTIONS] {
            let mut b = super::DocumentBuilder::new();
            b.start(super::airsync::APPLICATION_DATA);
            b.empty_tag(token);
            b.end();
            assert!(super::extract_calendar_fields(&b.finish().nodes).unsupported_recurrence);
        }
    }

    #[test]
    fn calendar_categories_validate_and_clear() {
        fn command(children: impl FnOnce(&mut super::DocumentBuilder)) -> super::Document {
            let mut b = super::DocumentBuilder::new();
            b.start(super::airsync::CHANGE);
            b.start(super::airsync::APPLICATION_DATA);
            children(&mut b);
            b.end();
            b.end();
            b.finish()
        }
        let absent = command(|_| {});
        assert_eq!(super::calendar_categories(&absent), Ok(None));
        let clear = command(|b| b.empty_tag(super::calendar::CATEGORIES));
        assert_eq!(super::calendar_categories(&clear), Ok(Some(vec![])));
        let valid = command(|b| {
            b.start(super::calendar::CATEGORIES);
            b.leaf(super::calendar::CATEGORY, "Work");
            b.leaf(super::calendar::CATEGORY, "Personal space");
            b.end();
        });
        assert_eq!(
            super::calendar_categories(&valid),
            Ok(Some(vec!["Personal space".into(), "Work".into()]))
        );
        let duplicate = command(|b| {
            b.start(super::calendar::CATEGORIES);
            b.leaf(super::calendar::CATEGORY, "Work");
            b.leaf(super::calendar::CATEGORY, "Work");
            b.end();
        });
        assert_eq!(super::calendar_categories(&duplicate), Err(()));
        let invalid = command(|b| {
            b.start(super::calendar::CATEGORIES);
            b.empty_tag(super::calendar::CATEGORY);
            b.end();
        });
        assert_eq!(super::calendar_categories(&invalid), Err(()));
        let unknown = command(|b| {
            b.start(super::calendar::CATEGORIES);
            b.leaf(super::calendar::SUBJECT, "not a category");
            b.end();
        });
        assert_eq!(super::calendar_categories(&unknown), Err(()));
    }

    #[test]
    fn calendar_busy_status_accepts_wire_values_and_rejects_invalid_updates() {
        let parse = |values: &[&str]| {
            let mut b = DocumentBuilder::new();
            b.start(airsync::APPLICATION_DATA);
            for value in values {
                b.leaf(calendar::BUSY_STATUS, *value);
            }
            b.end();
            super::extract_calendar_fields(&b.finish().nodes)
        };
        for value in 0..=3 {
            let result = parse(&[&value.to_string()]);
            assert_eq!(result.busy_status, Some(value));
            assert!(!result.unsupported_busy_status);
        }
        assert!(parse(&["4"]).unsupported_busy_status);
        assert!(parse(&["bad"]).unsupported_busy_status);
        assert!(parse(&["1", "2"]).unsupported_busy_status);
        assert_eq!(parse(&[]).busy_status, None);
    }

    #[test]
    fn calendar_body_distinguishes_clear_html_and_invalid_formats() {
        let make = |kind: Option<&str>, data: Option<&str>, duplicate: bool| {
            let mut b = DocumentBuilder::new();
            b.start(airsync::APPLICATION_DATA);
            b.start(airsync_base::BODY);
            if let Some(kind) = kind {
                b.leaf(airsync_base::TYPE, kind);
            }
            if let Some(data) = data {
                b.leaf(airsync_base::DATA, data);
            }
            if duplicate {
                b.leaf(airsync_base::DATA, "second");
            }
            b.end();
            b.end();
            super::extract_calendar_fields(&b.finish().nodes)
        };
        let html = make(Some("2"), Some("<p>hello</p>"), false);
        assert_eq!(html.body.as_deref(), Some("<p>hello</p>"));
        assert_eq!(html.body_type, Some(2));
        assert!(!html.unsupported_body);
        assert_eq!(make(Some("1"), None, false).body.as_deref(), Some(""));
        assert!(make(Some("3"), Some("bad"), false).unsupported_body);
        assert!(make(Some("1"), Some("bad"), true).unsupported_body);
        assert!(super::extract_calendar_fields(&[]).body.is_none());
    }

    #[test]
    fn calendar_subject_distinguishes_omission_and_clear() {
        assert_eq!(super::extract_calendar_fields(&[]).subject, None);
        let mut b = DocumentBuilder::new();
        b.empty_tag(calendar::SUBJECT);
        assert_eq!(
            super::extract_calendar_fields(&b.finish().nodes).subject,
            Some(String::new())
        );
        let mut b = DocumentBuilder::new();
        b.leaf(calendar::SUBJECT, "Meeting");
        assert_eq!(
            super::extract_calendar_fields(&b.finish().nodes)
                .subject
                .as_deref(),
            Some("Meeting")
        );
    }

    #[test]
    fn calendar_location_distinguishes_omission_clear_and_structured_name() {
        let mut b = DocumentBuilder::new();
        b.empty_tag(airsync_base::LOCATION);
        b.leaf(calendar::SUBJECT, "After clear");
        let doc = b.finish();
        let fields = super::extract_calendar_fields(&doc.nodes);
        assert_eq!(fields.location.as_deref(), Some(""));
        assert_eq!(fields.subject.as_deref(), Some("After clear"));
        assert!(!fields.unsupported_location);

        let mut b = DocumentBuilder::new();
        b.start(airsync_base::LOCATION);
        b.leaf(airsync_base::DISPLAY_NAME, "Room B");
        b.end();
        let fields = super::extract_calendar_fields(&b.finish().nodes);
        assert_eq!(fields.location.as_deref(), Some("Room B"));
        assert!(!fields.unsupported_location);

        let mut b = DocumentBuilder::new();
        b.start(airsync_base::LOCATION);
        b.leaf(airsync_base::tag(0x22, false), "Example Street");
        b.end();
        assert!(super::extract_calendar_fields(&b.finish().nodes).unsupported_location);
        let mut b = DocumentBuilder::new();
        b.start(airsync_base::LOCATION);
        b.leaf(airsync_base::LATITUDE, "43.65");
        b.leaf(airsync_base::LONGITUDE, "-79.38");
        b.end();
        let fields = super::extract_calendar_fields(&b.finish().nodes);
        assert_eq!(
            fields.location_coordinates,
            Some(("43.65".into(), "-79.38".into()))
        );
        assert!(!fields.unsupported_location);
        let mut b = DocumentBuilder::new();
        b.start(airsync_base::LOCATION);
        b.leaf(airsync_base::LATITUDE, "43");
        b.end();
        assert!(super::extract_calendar_fields(&b.finish().nodes).unsupported_location);
        for (lat, lon) in [("91", "0"), ("0", "181"), ("NaN", "0")] {
            let mut b = DocumentBuilder::new();
            b.start(airsync_base::LOCATION);
            b.leaf(airsync_base::LATITUDE, lat);
            b.leaf(airsync_base::LONGITUDE, lon);
            b.end();
            assert!(super::extract_calendar_fields(&b.finish().nodes).unsupported_location);
        }
        assert_eq!(super::extract_calendar_fields(&[]).location, None);
    }

    #[test]
    fn email_complete_time_uses_the_microsoft_wbxml_token() {
        assert_eq!(email::COMPLETE_TIME.code_page, 2);
        assert_eq!(email::COMPLETE_TIME.token, 0x3e);
    }

    #[test]
    fn mail_flags_require_the_specified_type_and_date_fields() {
        let date = "2026-11-14T12:00:00.000Z";
        let fields = vec![
            (email::FLAG_STATUS, "2"),
            (email::FLAG_TYPE, "Flag for follow up"),
            (tasks::START_DATE, date),
            (tasks::UTC_START_DATE, date),
            (tasks::DUE_DATE, date),
            (tasks::UTC_DUE_DATE, date),
        ];
        let command = |fields: &[(Token, &str)]| {
            let mut b = DocumentBuilder::new();
            b.start(airsync::CHANGE);
            b.start(airsync::APPLICATION_DATA);
            b.start(email::FLAG);
            for &(token, value) in fields {
                b.leaf(token, value);
            }
            b.end();
            b.end();
            b.end();
            b.finish()
        };
        assert!(super::valid_mail_flag(&command(&fields), Some(2)));
        for missing in 0..fields.len() {
            let mut bad = fields.clone();
            bad.remove(missing);
            assert!(
                !super::valid_mail_flag(&command(&bad), Some(2)),
                "missing field {missing}"
            );
        }
        for invalid in [
            "invalid",
            "2026-02-30T12:00:00.000Z",
            "2026-11-14T12:00:00+02:00",
        ] {
            let mut bad = fields.clone();
            bad[2].1 = invalid;
            assert!(!super::valid_mail_flag(&command(&bad), Some(2)));
        }
        let mut duplicate = fields.clone();
        duplicate.push(fields[0]);
        assert!(!super::valid_mail_flag(&command(&duplicate), Some(2)));
        let mut duplicate = fields.clone();
        duplicate.push(fields[1]);
        assert!(!super::valid_mail_flag(&command(&duplicate), Some(2)));
        let mut invalid_reminder = fields.clone();
        invalid_reminder.push((tasks::REMINDER_TIME, "invalid"));
        assert!(!super::valid_mail_flag(
            &command(&invalid_reminder),
            Some(2)
        ));
        let mut invalid_reminder = fields.clone();
        invalid_reminder.push((tasks::REMINDER_SET, "3"));
        assert!(!super::valid_mail_flag(
            &command(&invalid_reminder),
            Some(2)
        ));
        let completed = [
            (email::FLAG_STATUS, "2"),
            (email::FLAG_TYPE, "Flag for follow up"),
            (tasks::DATE_COMPLETED, date),
            (email::COMPLETE_TIME, date),
        ];
        assert!(super::valid_mail_flag(&command(&completed), Some(2)));
        assert!(super::valid_mail_flag(
            &command(&[(email::FLAG_STATUS, "0")]),
            Some(0)
        ));
        assert!(super::valid_mail_flag(&command(&[]), Some(0)));
        assert!(!super::valid_mail_flag(
            &command(&[(email::FLAG_STATUS, "1")]),
            Some(1)
        ));
    }

    #[test]
    fn body_preferences_keep_separate_formats_and_reject_invalid_fields() {
        let build = |types: &[&str], preview: &str| {
            let mut b = DocumentBuilder::new();
            b.start(airsync::OPTIONS);
            for body_type in types {
                b.start(airsync_base::BODY_PREFERENCE);
                b.leaf(airsync_base::TYPE, *body_type);
                b.leaf(
                    airsync_base::TRUNCATION_SIZE,
                    if *body_type == "2" { "50" } else { "20" },
                );
                b.leaf(
                    airsync_base::ALL_OR_NONE,
                    if *body_type == "2" { "1" } else { "0" },
                );
                b.leaf(airsync_base::PREVIEW, preview);
                b.end();
            }
            b.end();
            b.finish()
        };
        let prefs = super::body_preferences(&build(&["2", "1"], "3")).unwrap();
        assert_eq!(prefs.len(), 2);
        assert_eq!(prefs[0].body_type, 2);
        assert_eq!(prefs[0].truncation_size, Some(50));
        assert!(prefs[0].all_or_none);
        assert_eq!(prefs[1].body_type, 1);
        assert_eq!(prefs[1].truncation_size, Some(20));
        assert!(!prefs[1].all_or_none);
        assert_eq!(prefs[1].preview, Some(3));
        for (types, preview) in [
            (vec!["1", "1"], "0"),
            (vec!["0"], "0"),
            (vec!["5"], "0"),
            (vec!["1"], "256"),
            (vec!["1"], "-1"),
        ] {
            assert!(super::body_preferences(&build(&types, preview)).is_err());
        }
        let mut b = DocumentBuilder::new();
        b.start(airsync::OPTIONS);
        b.start(airsync_base::BODY_PREFERENCE);
        b.leaf(airsync_base::PREVIEW, "3");
        b.end();
        b.end();
        assert!(super::body_preferences(&b.finish()).is_err());
    }

    #[test]
    fn parses_calendar_add_with_two_attendees() {
        // Exercises extract_calendar_fields()'s Attendees accumulator --
        // two sibling Attendee blocks must produce two separate
        // AttendeeField entries, not get merged or dropped, and
        // AttendeeType 2 must set `optional` while the default (absent)
        // stays Required.
        let mut builder = DocumentBuilder::new();
        builder.start(airsync::SYNC);
        builder.start(airsync::COLLECTIONS);
        builder.start(airsync::COLLECTION);
        builder.leaf(airsync::SYNC_KEY, "1");
        builder.leaf(airsync::COLLECTION_ID, "cal_x");
        builder.start(airsync::COMMANDS);
        builder.start(airsync::ADD);
        builder.leaf(airsync::CLIENT_ID, "client-cal-1");
        builder.start(airsync::APPLICATION_DATA);
        builder.leaf(calendar::SUBJECT, "Standup");
        builder.start(calendar::ATTENDEES);
        builder.start(calendar::ATTENDEE);
        builder.leaf(calendar::ATTENDEE_EMAIL, "chris@fourthcoffee.com");
        builder.leaf(calendar::ATTENDEE_NAME, "Chris Gray");
        builder.end();
        builder.start(calendar::ATTENDEE);
        builder.leaf(calendar::ATTENDEE_EMAIL, "pat@fourthcoffee.com");
        builder.leaf(calendar::ATTENDEE_TYPE, "2");
        builder.end();
        builder.end();
        builder.end();
        builder.end();
        builder.end();
        builder.end();
        builder.end();
        builder.end();
        builder.end();

        let collections = sync_collections(&builder.finish());

        let command = &collections[0].commands[0];
        assert_eq!(command.calendar.subject.as_deref(), Some("Standup"));
        assert_eq!(command.calendar.attendees.len(), 2);
        assert_eq!(
            command.calendar.attendees[0].email,
            "chris@fourthcoffee.com"
        );
        assert_eq!(
            command.calendar.attendees[0].name.as_deref(),
            Some("Chris Gray")
        );
        assert!(!command.calendar.attendees[0].optional);
        assert_eq!(command.calendar.attendees[1].email, "pat@fourthcoffee.com");
        assert_eq!(command.calendar.attendees[1].name, None);
        assert!(command.calendar.attendees[1].optional);
    }

    #[test]
    fn parses_search_request_with_class_free_text_and_range() {
        // Mirrors how a real device wraps FreeText in an And{} alongside
        // a Class filter -- search_request() must still find both flat
        // elements regardless of the surrounding And container, since it
        // uses find_text_after rather than a strict nested parse.
        let mut builder = DocumentBuilder::new();
        builder.start(search::SEARCH);
        builder.start(search::STORE);
        builder.leaf(search::NAME, "Mailbox");
        builder.start(search::QUERY);
        builder.start(search::AND);
        builder.leaf(airsync::CLASS, "Email");
        builder.leaf(search::FREE_TEXT, "invite");
        builder.end();
        builder.end();
        builder.start(search::OPTIONS);
        builder.leaf(search::RANGE, "0-9");
        builder.end();
        builder.end();
        builder.end();

        let request = search_request(&builder.finish()).expect("search request should parse");
        assert_eq!(request.store_name, "Mailbox");
        assert_eq!(request.class.as_deref(), Some("Email"));
        assert_eq!(request.free_text.as_deref(), Some("invite"));
        assert_eq!(request.range, Some((0, 9)));
    }

    #[test]
    fn parses_search_or_under_and_and_rejects_invalid_groups() {
        let document = |valid_parent: bool, include_term: bool| {
            let mut b = DocumentBuilder::new();
            b.start(search::SEARCH);
            b.start(search::STORE);
            b.leaf(search::NAME, "Mailbox");
            b.start(search::QUERY);
            if valid_parent {
                b.start(search::AND);
            }
            b.leaf(search::FREE_TEXT, "outer");
            b.start(search::OR);
            if include_term {
                b.leaf(search::FREE_TEXT, "inner");
            }
            b.end();
            if valid_parent {
                b.end();
            }
            b.end();
            b.end();
            b.end();
            b.finish()
        };
        let valid = search_request(&document(true, true)).unwrap();
        assert!(!valid.unsupported_query);
        assert_eq!(valid.free_texts, vec!["outer"]);
        assert_eq!(valid.or_terms.unwrap().texts, vec!["inner"]);
        assert!(
            search_request(&document(false, true))
                .unwrap()
                .unsupported_query
        );
        assert!(
            search_request(&document(true, false))
                .unwrap()
                .unsupported_query
        );
    }

    #[test]
    fn validates_search_result_ranges() {
        let request = |range: &str| {
            let mut b = DocumentBuilder::new();
            b.start(search::SEARCH);
            b.start(search::STORE);
            b.leaf(search::NAME, "Mailbox");
            b.start(search::OPTIONS);
            b.leaf(search::RANGE, range);
            b.end();
            b.end();
            b.end();
            search_request(&b.finish()).unwrap()
        };
        assert_eq!(request("10-19").range, Some((10, 19)));
        assert_eq!(request("0-100").range_error, Some("12"));
        assert_eq!(request("7-6").range_error, Some("2"));
        assert_eq!(request("-1-0").range_error, Some("2"));
        assert_eq!(request("0-9223372036854775807").range_error, Some("12"));
        assert_eq!(request("0-9223372036854775808").range_error, Some("2"));
    }

    #[test]
    fn validates_mailbox_search_date_comparisons() {
        let document = |duplicate: bool, wrong_order: bool, value: &str| {
            let mut b = DocumentBuilder::new();
            b.start(search::SEARCH);
            b.start(search::STORE);
            b.leaf(search::NAME, "Mailbox");
            b.start(search::QUERY);
            b.start(search::AND);
            for _ in 0..if duplicate { 2 } else { 1 } {
                b.start(search::GREATER_THAN);
                if wrong_order {
                    b.leaf(search::VALUE, value);
                    b.empty_tag(email::DATE_RECEIVED);
                } else {
                    b.empty_tag(email::DATE_RECEIVED);
                    b.leaf(search::VALUE, value);
                }
                b.end();
            }
            b.end();
            b.end();
            b.end();
            b.end();
            b.finish()
        };
        let valid = search_request(&document(false, false, "2020-01-01T00:00:00Z")).unwrap();
        assert!(!valid.unsupported_query);
        assert_eq!(
            valid.received_after.as_deref(),
            Some("2020-01-01T00:00:00Z")
        );
        for invalid in [
            document(true, false, "2020-01-01T00:00:00Z"),
            document(false, true, "2020-01-01T00:00:00Z"),
            document(false, false, "not a date"),
        ] {
            assert!(search_request(&invalid).unwrap().unsupported_query);
        }
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
        assert_eq!(collections[0].body_preferences[0].body_type, 1);
        assert_eq!(
            collections[0].body_preferences[0].truncation_size,
            Some(500)
        );
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
        assert!(collections[0].body_preferences.is_empty());
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
    #[test]
    fn sync_window_and_delete_defaults_follow_the_protocol() {
        let mut b = DocumentBuilder::new();
        b.start(airsync::SYNC);
        b.start(airsync::COLLECTIONS);
        b.start(airsync::COLLECTION);
        b.leaf(airsync::SYNC_KEY, "1");
        b.leaf(airsync::COLLECTION_ID, "folder");
        b.end();
        b.end();
        b.end();
        assert_eq!(sync_collections(&b.finish())[0].window_size, 100);

        for (window, expected) in [("0", 512), ("513", 512), ("19", 19)] {
            for mode in [None, Some(""), Some("0"), Some("1")] {
                let mut b = DocumentBuilder::new();
                b.start(airsync::SYNC);
                b.start(airsync::COLLECTIONS);
                b.start(airsync::COLLECTION);
                b.leaf(airsync::SYNC_KEY, "1");
                b.leaf(airsync::COLLECTION_ID, "folder");
                if let Some(mode) = mode {
                    if mode.is_empty() {
                        b.empty_tag(airsync::DELETES_AS_MOVES);
                    } else {
                        b.leaf(airsync::DELETES_AS_MOVES, mode);
                    }
                }
                b.leaf(airsync::WINDOW_SIZE, window);
                b.end();
                b.end();
                b.end();
                let collections = sync_collections(&b.finish());
                assert_eq!(collections[0].window_size, expected);
                assert_eq!(collections[0].deletes_as_moves, mode != Some("0"));
            }
        }
    }
}
