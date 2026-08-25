#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectionKind {
    Mail,
    Contacts,
    Calendar,
    Tasks,
    Notes,
    Files,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub kind: CollectionKind,
    pub role: Option<String>,
    pub folder_type: u8,
}

pub mod eas_folder_type {
    pub const INBOX: u8 = 2;
    pub const DRAFTS: u8 = 3;
    pub const WASTEBASKET: u8 = 4;
    pub const SENTMAIL: u8 = 5;
    pub const APPOINTMENT: u8 = 8;
    pub const CONTACT: u8 = 9;
    /// Verified against Z-Push's own zpushdefs.php
    /// (`SYNC_FOLDER_TYPE_NOTE`), not assumed.
    pub const NOTE: u8 = 10;
    pub const USER_MAIL: u8 = 12;
    pub const USER_APPOINTMENT: u8 = 13;
    pub const USER_CONTACT: u8 = 14;
    /// Verified against Z-Push's own zpushdefs.php
    /// (`SYNC_FOLDER_TYPE_TASK`/`SYNC_FOLDER_TYPE_USER_TASK`), same as NOTE.
    pub const TASK: u8 = 7;
    pub const USER_TASK: u8 = 15;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    pub id: String,
    pub mailbox_ids: Vec<String>,
    pub subject: String,
    pub received_at: Option<String>,
    pub keywords: Vec<String>,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub read: bool,
    pub body: Option<EmailBody>,
    pub attachments: Vec<EmailAttachment>,
    /// The blobId of the message's own raw RFC822 source (JMAP Email
    /// objects expose this directly, distinct from any attachment's own
    /// blobId) -- needed to answer a BodyPreference Type=4 (MIME) fetch,
    /// which real EAS clients (confirmed live: a real iPad) request when
    /// actually opening a message, as opposed to the plain/HTML type the
    /// list-sync view asks for.
    pub blob_id: Option<String>,
    /// JMAP's own `Email/threadId` -- same JMAP thread = same EAS
    /// conversation, exactly the right semantics for `Email2:
    /// ConversationId`. See `eas_conversation_id` in `activesync.rs` for
    /// why the raw thread id string is never sent directly (a previous
    /// attempt tried that -- commit `d199d37` -- and caused a real,
    /// device-visible sync error).
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailAttachment {
    pub blob_id: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailBody {
    pub body_type: EmailBodyType,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailBodyType {
    Plain,
    Html,
}

impl EmailBodyType {
    pub fn eas_value(self) -> &'static str {
        match self {
            Self::Plain => "1",
            Self::Html => "2",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contact {
    pub id: String,
    pub address_book_ids: Vec<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub file_as: Option<String>,
    pub emails: Vec<String>,
    pub mobile_phone: Option<String>,
    pub home_phone: Option<String>,
    pub business_phone: Option<String>,
    pub company_name: Option<String>,
    pub job_title: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CalendarEvent {
    pub id: String,
    pub calendar_ids: Vec<String>,
    pub title: String,
    pub location: Option<String>,
    /// UTC, compact EAS DateTime form (already converted from JSCalendar's
    /// local `start` + `timeZone` -- see jmap::client's
    /// `local_to_utc_eas`). None if `start` was missing or unparseable.
    pub start_utc: Option<String>,
    pub end_utc: Option<String>,
    pub all_day: bool,
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
    pub attendees: Vec<Attendee>,
    /// Whether the authenticated user IS the organizer -- can't be derived
    /// from `organizer_email` alone at model-construction time (that
    /// comparison needs the auth session), so `calendar_events_in_calendar`
    /// sets this explicitly after mapping. Drives MeetingStatus (1 vs 3) in
    /// `write_calendar_command`.
    pub is_organizer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Attendee {
    pub email: String,
    pub name: Option<String>,
    /// JSCalendar `participationStatus` ("needs-action"/"accepted"/
    /// "declined"/"tentative"), kept as the raw JMAP string and mapped to
    /// MS-ASCAL's AttendeeStatus ints at the WBXML-writing call site --
    /// see `write_calendar_command`'s own comment for the confirmed enum.
    pub participation_status: Option<String>,
    /// True when this participant's JSCalendar `roles` includes
    /// `"optional"` -- maps to MS-ASCAL AttendeeType 2 (Optional) instead
    /// of the default 1 (Required).
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub completed: bool,
    pub due: Option<String>,
}

/// `id` is the note's permanent stable id (see jmap/notes.rs), never the
/// underlying JMAP Email id -- MS-ASCMD requires a ServerId to stay
/// constant across edits, which the JMAP object backing a note (an Email,
/// immutable once imported) structurally cannot do on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub body: String,
    pub body_type: EmailBodyType,
    pub modified: Option<String>,
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateToken(pub String);
