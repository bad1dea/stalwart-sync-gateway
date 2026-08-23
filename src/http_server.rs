use std::time::Duration;

use axum::{
    routing::{get, options, post},
    Router,
};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

use crate::{
    activesync, autodiscover,
    config::Config,
    jmap::client::JmapClient,
    metrics::{self, Metrics},
    state,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub jmap: JmapClient,
    pub metrics: Metrics,
    pub state: state::SharedStateStore,
}

pub async fn serve(config: Config) -> anyhow::Result<()> {
    let metrics = Metrics::new()?;
    let jmap = JmapClient::new(config.clone())?;
    let state = state::new_store(&config).await?;
    let app_state = AppState {
        config: config.clone(),
        jmap,
        metrics,
        state,
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics::handler))
        .route(
            "/Microsoft-Server-ActiveSync",
            options(activesync::options_handler).post(activesync::post_handler),
        )
        .route(
            "/Autodiscover/Autodiscover.xml",
            post(autodiscover::handler),
        )
        .with_state(app_state)
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &http::Request<_>| {
                tracing::info_span!(
                    "http_request",
                    method = %request.method(),
                    path = %request.uri().path()
                )
            }),
        );

    let listener = TcpListener::bind(config.listen_addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn readyz(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<&'static str, http::StatusCode> {
    state
        .jmap
        .unauthenticated_probe()
        .await
        .map(|_| "ready\n")
        .map_err(|error| {
            tracing::warn!(%error, "readiness probe failed");
            http::StatusCode::SERVICE_UNAVAILABLE
        })
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
}
