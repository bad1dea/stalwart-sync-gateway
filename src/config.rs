use std::{env, net::SocketAddr};

use anyhow::{bail, Context};
use url::Url;

#[derive(Clone, Debug)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub stalwart_jmap_url: Url,
    pub stalwart_tls_verify: bool,
    pub eas_public_url: Url,
    pub state_backend: StateBackend,
    pub state_sqlite_path: String,
    pub log_level: String,
    pub max_wbxml_bytes: usize,
    /// Heartbeat limits advertised via Ping Status 5, seconds.
    pub ping_max_seconds: u64,
    pub ping_min_seconds: u64,
    pub ping_max_concurrent: usize,
    /// After answering Status 2, a device that pings again without having
    /// synced is not re-answered for this long (loop protection); the Ping
    /// then re-checks when the window ends instead of waiting for an event.
    pub ping_resend_guard_seconds: u64,
    /// How long a per-user JMAP EventSource stays open with no Ping activity.
    pub push_idle_seconds: u64,
    /// What a mail Sync sends for a held message that has aged out of the
    /// device's FilterType window (the message stays on the server).
    pub mail_aged_out: AgedOut,
}

/// Cleanup of held mail older than the device's FilterType window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgedOut {
    /// [MS-ASCMD] SoftDelete: remove from the device, not from the server.
    SoftDelete,
    /// An ordinary Delete, as Z-Push's generic backends send.
    Delete,
    /// Leave it on the device (behaviour before this option existed).
    Keep,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateBackend {
    Memory,
    Sqlite,
    Redis,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let listen_addr = env_or("LISTEN_ADDR", "0.0.0.0:8080")
            .parse()
            .context("LISTEN_ADDR must be host:port")?;
        let stalwart_jmap_url = Url::parse(&env_or(
            "STALWART_JMAP_URL",
            "http://stalwart:8080/.well-known/jmap",
        ))
        .context("STALWART_JMAP_URL must be a URL")?;
        let eas_public_url = Url::parse(&env_or(
            "EAS_PUBLIC_URL",
            "https://mail.example.com/Microsoft-Server-ActiveSync",
        ))
        .context("EAS_PUBLIC_URL must be a URL")?;

        let state_backend = match env_or("STATE_BACKEND", "sqlite")
            .to_ascii_lowercase()
            .as_str()
        {
            "memory" => StateBackend::Memory,
            "sqlite" => StateBackend::Sqlite,
            "redis" => StateBackend::Redis,
            other => bail!("unsupported STATE_BACKEND {other:?}"),
        };

        Ok(Self {
            listen_addr,
            stalwart_jmap_url,
            stalwart_tls_verify: env_bool("STALWART_TLS_VERIFY", true),
            eas_public_url,
            state_backend,
            state_sqlite_path: env_or("STATE_SQLITE_PATH", "/data/state.db"),
            log_level: env_or("LOG_LEVEL", &env_or("RUST_LOG", "info")),
            max_wbxml_bytes: env_or("MAX_WBXML_BYTES", "1048576")
                .parse()
                .context("MAX_WBXML_BYTES must be an integer")?,
            ping_max_seconds: env_or("PING_MAX_SECONDS", "1800")
                .parse()
                .context("PING_MAX_SECONDS must be an integer")?,
            ping_min_seconds: env_or("PING_MIN_SECONDS", "60")
                .parse()
                .context("PING_MIN_SECONDS must be an integer")?,
            ping_max_concurrent: env_or("PING_MAX_CONCURRENT", "200")
                .parse()
                .context("PING_MAX_CONCURRENT must be an integer")?,
            ping_resend_guard_seconds: env_or("PING_RESEND_GUARD_SECONDS", "15")
                .parse()
                .context("PING_RESEND_GUARD_SECONDS must be an integer")?,
            push_idle_seconds: env_or("PUSH_IDLE_SECONDS", "1800")
                .parse()
                .context("PUSH_IDLE_SECONDS must be an integer")?,
            mail_aged_out: match env_or("MAIL_AGED_OUT", "softdelete")
                .to_ascii_lowercase()
                .as_str()
            {
                "softdelete" => AgedOut::SoftDelete,
                "delete" => AgedOut::Delete,
                "keep" => AgedOut::Keep,
                other => bail!("MAIL_AGED_OUT must be softdelete, delete or keep, not {other:?}"),
            },
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn env_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .and_then(|v| match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}
