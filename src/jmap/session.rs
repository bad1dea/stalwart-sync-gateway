use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JmapSession {
    pub capabilities: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub accounts: BTreeMap<String, JmapAccount>,
    #[serde(default)]
    pub primary_accounts: BTreeMap<String, String>,
    pub username: Option<String>,
    pub api_url: String,
    pub download_url: String,
    pub upload_url: String,
    pub event_source_url: Option<String>,
    pub state: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JmapAccount {
    pub name: String,
    pub is_personal: bool,
    pub is_read_only: bool,
    #[serde(default)]
    pub account_capabilities: BTreeMap<String, serde_json::Value>,
}

impl JmapSession {
    pub fn primary_account_for(&self, capability: &str) -> Option<&str> {
        self.primary_accounts.get(capability).map(String::as_str)
    }
}
