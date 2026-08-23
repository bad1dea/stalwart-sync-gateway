#![allow(dead_code)]

mod activesync;
mod autodiscover;
mod config;
mod http_server;
mod jmap;
mod metrics;
mod model;
mod state;
mod wbxml;

use anyhow::Context;
use config::Config;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    init_tracing(&config);

    tracing::info!(
        listen_addr = %config.listen_addr,
        stalwart_jmap_url = %config.stalwart_jmap_url,
        "starting stalwart sync gateway"
    );

    http_server::serve(config)
        .await
        .context("HTTP server exited")?;
    Ok(())
}

fn init_tracing(config: &Config) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new(&config.log_level))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json())
        .init();
}
