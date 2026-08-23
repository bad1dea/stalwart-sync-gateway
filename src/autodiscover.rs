use axum::{extract::State, http::StatusCode, response::IntoResponse};

use crate::http_server::AppState;

pub async fn handler(State(state): State<AppState>) -> impl IntoResponse {
    let url = state.config.eas_public_url.as_str();
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006">
  <Response xmlns="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006">
    <Culture>en:us</Culture>
    <User>
      <DisplayName></DisplayName>
      <EMailAddress></EMailAddress>
    </User>
    <Action>
      <Settings>
        <Server>
          <Type>MobileSync</Type>
          <Url>{url}</Url>
          <Name>{url}</Name>
        </Server>
      </Settings>
    </Action>
  </Response>
</Autodiscover>
"#
    );
    (
        StatusCode::OK,
        [("Content-Type", "text/xml; charset=utf-8")],
        body,
    )
}
