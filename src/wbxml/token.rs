pub const SWITCH_PAGE: u8 = 0x00;
pub const END: u8 = 0x01;
pub const STR_I: u8 = 0x03;
pub const OPAQUE: u8 = 0xC3;
pub const WITH_CONTENT: u8 = 0x40;
pub const WITH_ATTRIBUTES: u8 = 0x80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub code_page: u8,
    pub token: u8,
    pub has_content: bool,
    pub has_attributes: bool,
}

impl Token {
    pub fn from_byte(code_page: u8, byte: u8) -> Self {
        Self {
            code_page,
            token: byte & 0x3f,
            has_content: byte & WITH_CONTENT != 0,
            has_attributes: byte & WITH_ATTRIBUTES != 0,
        }
    }

    pub fn to_byte(self) -> u8 {
        self.token
            | if self.has_content { WITH_CONTENT } else { 0 }
            | if self.has_attributes {
                WITH_ATTRIBUTES
            } else {
                0
            }
    }
}
