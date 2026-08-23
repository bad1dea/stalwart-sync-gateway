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
