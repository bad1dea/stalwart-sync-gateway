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
    pub const USER_MAIL: u8 = 12;
    pub const USER_APPOINTMENT: u8 = 13;
    pub const USER_CONTACT: u8 = 14;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub id: String,
    pub address_book_ids: Vec<String>,
    pub full_name: Option<String>,
    pub emails: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    pub id: String,
    pub calendar_ids: Vec<String>,
    pub title: String,
    pub start: Option<String>,
    pub duration: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub completed: bool,
    pub due: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub body: String,
    pub modified: Option<String>,
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateToken(pub String);
