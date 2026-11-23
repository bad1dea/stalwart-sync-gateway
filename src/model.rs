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
    pub draft_revision: Option<String>,
    pub mailbox_ids: Vec<String>,
    pub subject: String,
    pub received_at: Option<String>,
    pub keywords: Vec<String>,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub reply_to: String,
    pub importance: u8,
    pub preview: Option<String>,
    pub read: bool,
    pub body: Option<EmailBody>,
    pub smime: Option<SmimeKind>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmimeKind {
    Opaque,
    MultipartSigned,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MailFlag {
    pub status: u8,
    pub fields: Vec<(u8, u8, String)>,
}

pub const FLAG_KEYWORD_PREFIX: &str = "easflag-";

impl MailFlag {
    pub fn keywords(&self) -> anyhow::Result<Vec<String>> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > 3200 {
            anyhow::bail!("flag metadata is too large");
        }
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(hex
            .as_bytes()
            .chunks(110)
            .enumerate()
            .map(|(index, chunk)| {
                format!(
                    "{FLAG_KEYWORD_PREFIX}{index:04x}-{}",
                    std::str::from_utf8(chunk).unwrap()
                )
            })
            .collect())
    }

    fn from_keywords<'a>(keywords: impl Iterator<Item = &'a str>) -> Option<Self> {
        let mut parts = std::collections::BTreeMap::new();
        for keyword in keywords {
            let Some(part) = keyword.strip_prefix(FLAG_KEYWORD_PREFIX) else {
                continue;
            };
            let (index, hex) = part.split_once('-')?;
            if index.len() != 4 || hex.len() % 2 != 0 || hex.len() > 110 {
                return None;
            }
            let index = usize::from_str_radix(index, 16).ok()?;
            if parts.insert(index, hex).is_some() {
                return None;
            }
        }
        if parts.is_empty() {
            return None;
        }
        let mut bytes = Vec::new();
        for (expected, (index, hex)) in parts.into_iter().enumerate() {
            if expected != index {
                return None;
            }
            for pair in hex.as_bytes().chunks(2) {
                bytes.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
            }
        }
        serde_json::from_slice(&bytes).ok()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmailState {
    pub read: bool,
    pub flagged: bool,
    pub flag: Option<MailFlag>,
    pub draft_revision: Option<String>,
}

impl EmailState {
    pub fn from_keywords(keywords: &std::collections::BTreeMap<String, bool>) -> Self {
        let read = keywords.get("$seen").copied().unwrap_or(false);
        let flagged = keywords.get("$flagged").copied().unwrap_or(false);
        Self::from_keyword_iter(
            read,
            flagged,
            keywords
                .iter()
                .filter(|(_, value)| **value)
                .map(|(key, _)| key.as_str()),
        )
    }

    fn from_keyword_iter<'a>(
        read: bool,
        flagged: bool,
        keywords: impl Iterator<Item = &'a str>,
    ) -> Self {
        let flag = MailFlag::from_keywords(keywords)
            .filter(|flag| (flag.status == 2 && flagged) || (flag.status == 1 && !flagged));
        Self {
            read,
            flagged,
            flag,
            draft_revision: None,
        }
    }

    pub fn fingerprint(&self) -> String {
        let mut value = if self.read {
            "1".to_owned()
        } else {
            "0".to_owned()
        };
        if self.flagged {
            value.push_str(";flagged");
        }
        if let Some(flag) = &self.flag {
            if let Ok(encoded) = serde_json::to_vec(flag) {
                value.push_str(";flag:");
                value.push_str(
                    &uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, &encoded).to_string(),
                );
            }
        }
        if let Some(revision) = &self.draft_revision {
            value.push_str(";draft:");
            value.push_str(revision);
        }
        value
    }
}

impl Email {
    pub fn message_state(&self) -> EmailState {
        let flagged = self.keywords.iter().any(|k| k == "$flagged");
        let mut state = EmailState::from_keyword_iter(
            self.read,
            flagged,
            self.keywords.iter().map(String::as_str),
        );
        state.draft_revision.clone_from(&self.draft_revision);
        state
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftAttachment {
    pub client_id: String,
    pub name: String,
    pub content_type: String,
    pub content: Vec<u8>,
    pub content_id: Option<String>,
    pub content_location: Option<String>,
    pub inline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailAttachment {
    pub blob_id: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub content_location: Option<String>,
    pub inline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailBody {
    pub body_type: EmailBodyType,
    pub value: String,
    pub is_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailBodyType {
    Plain,
    Html,
    Rtf,
}

impl EmailBodyType {
    pub fn eas_value(self) -> &'static str {
        match self {
            Self::Plain => "1",
            Self::Html => "2",
            Self::Rtf => "3",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contact {
    pub id: String,
    pub address_book_ids: Vec<String>,
    pub first_name: Option<String>,
    pub middle_name: Option<String>,
    pub last_name: Option<String>,
    pub suffix: Option<String>,
    pub birthday: Option<String>,
    pub anniversary: Option<String>,
    pub file_as: Option<String>,
    pub emails: Vec<String>,
    pub mobile_phone: Option<String>,
    pub home_phone: Option<String>,
    pub business_phone: Option<String>,
    pub company_name: Option<String>,
    pub job_title: Option<String>,
    pub business_address: Option<ContactAddress>,
    pub home_address: Option<ContactAddress>,
    pub other_address: Option<ContactAddress>,
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ContactAddress {
    pub city: Option<String>,
    pub country: Option<String>,
    pub postal_code: Option<String>,
    pub state: Option<String>,
    pub street: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CalendarEvent {
    pub id: String,
    pub uid: Option<String>,
    pub calendar_ids: Vec<String>,
    pub title: String,
    pub updated_utc: Option<String>,
    pub description: Option<EmailBody>,
    pub location: Option<String>,
    pub location_coordinates: Option<(String, String)>,
    /// UTC, compact EAS DateTime form (already converted from JSCalendar's
    /// local `start` + `timeZone` -- see jmap::client's
    /// `local_to_utc_eas`). None if `start` was missing or unparseable.
    pub start_utc: Option<String>,
    pub end_utc: Option<String>,
    pub all_day: bool,
    pub busy_status: Option<u8>,
    pub sensitivity: u8,
    pub reminder_minutes: Option<u32>,
    pub categories: Vec<String>,
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
    pub attendees: Vec<Attendee>,
    /// Whether the authenticated user IS the organizer -- can't be derived
    /// from `organizer_email` alone at model-construction time (that
    /// comparison needs the auth session), so `calendar_events_in_calendar`
    /// sets this explicitly after mapping. Drives MeetingStatus (1 vs 3) in
    /// `write_calendar_command`.
    pub is_organizer: bool,
    pub cancelled: bool,
    pub response_type: Option<u8>,
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
    pub description: Option<EmailBody>,
    pub completed: bool,
    pub due: Option<String>,
    pub due_local: Option<String>,
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

#[cfg(test)]
mod flag_tests {
    use super::*;

    #[test]
    fn flag_keywords_round_trip_and_detect_missing_chunks() {
        let flag = MailFlag {
            status: 2,
            fields: vec![(9, 32, "Follow up 🐱".repeat(40))],
        };
        let keys = flag.keywords().unwrap();
        assert!(keys.len() > 2);
        assert!(keys.iter().all(|key| key.len() <= 128));
        assert_eq!(
            MailFlag::from_keywords(keys.iter().map(String::as_str)),
            Some(flag)
        );
        assert_eq!(
            MailFlag::from_keywords(keys.iter().skip(1).map(String::as_str)),
            None
        );
    }
}
