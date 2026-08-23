use axum::{extract::State, response::IntoResponse};
use prometheus::{Encoder, IntCounterVec, Registry, TextEncoder};

use crate::http_server::AppState;

#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    pub eas_requests_total: IntCounterVec,
    pub jmap_requests_total: IntCounterVec,
}

impl Metrics {
    pub fn new() -> anyhow::Result<Self> {
        let registry = Registry::new();
        let eas_requests_total = IntCounterVec::new(
            prometheus::Opts::new("eas_requests_total", "ActiveSync requests"),
            &["command", "status"],
        )?;
        let jmap_requests_total = IntCounterVec::new(
            prometheus::Opts::new("jmap_requests_total", "JMAP HTTP requests"),
            &["operation", "status"],
        )?;
        registry.register(Box::new(eas_requests_total.clone()))?;
        registry.register(Box::new(jmap_requests_total.clone()))?;
        Ok(Self {
            registry,
            eas_requests_total,
            jmap_requests_total,
        })
    }

    pub fn encode(&self) -> anyhow::Result<String> {
        let encoder = TextEncoder::new();
        let mut bytes = Vec::new();
        encoder.encode(&self.registry.gather(), &mut bytes)?;
        Ok(String::from_utf8(bytes)?)
    }
}

pub async fn handler(State(state): State<AppState>) -> impl IntoResponse {
    match state.metrics.encode() {
        Ok(body) => (http::StatusCode::OK, body).into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to encode metrics");
            (
                http::StatusCode::INTERNAL_SERVER_ERROR,
                "metrics unavailable\n",
            )
                .into_response()
        }
    }
}
