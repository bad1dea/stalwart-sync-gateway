use crate::jmap::session::JmapSession;

pub const CORE: &str = "urn:ietf:params:jmap:core";
pub const MAIL: &str = "urn:ietf:params:jmap:mail";
pub const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
pub const CONTACTS: &str = "urn:ietf:params:jmap:contacts";
pub const CALENDARS: &str = "urn:ietf:params:jmap:calendars";
pub const BLOB: &str = "urn:ietf:params:jmap:blob";
pub const FILES: &str = "urn:ietf:params:jmap:filenode";
pub const WEBSOCKET: &str = "urn:ietf:params:jmap:websocket";
pub const VACATION_RESPONSE: &str = "urn:ietf:params:jmap:vacationresponse";
pub const PRINCIPALS: &str = "urn:ietf:params:jmap:principals";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatewayCapabilities {
    pub mail: bool,
    pub submission: bool,
    pub contacts: bool,
    pub calendar: bool,
    pub tasks: bool,
    pub notes: bool,
    pub files: bool,
    pub push: bool,
    pub websocket: bool,
    pub event_source: bool,
    pub vacation_response: bool,
}

impl GatewayCapabilities {
    pub fn from_session(session: &JmapSession) -> Self {
        let has = |cap: &str| session.capabilities.contains_key(cap);
        Self {
            mail: has(MAIL),
            submission: has(SUBMISSION),
            contacts: has(CONTACTS),
            calendar: has(CALENDARS),
            tasks: has(CALENDARS),
            notes: has(FILES) || has(MAIL),
            files: has(FILES) || has(BLOB),
            push: session.event_source_url.is_some(),
            websocket: has(WEBSOCKET),
            event_source: session.event_source_url.is_some(),
            vacation_response: has(VACATION_RESPONSE),
        }
    }
}
