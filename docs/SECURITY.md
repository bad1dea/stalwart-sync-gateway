# Security

M0 controls:

- Basic auth credentials are used only to authenticate to Stalwart JMAP.
- Passwords and authorization headers are not logged.
- WBXML request size is bounded by `MAX_WBXML_BYTES`.
- WBXML nesting and token counts are bounded.
- Unsupported WBXML attributes and string tables are rejected.
- TLS verification is enabled by default for Stalwart.

Planned:

- request body limits at Axum layer
- authentication rate limits
- attachment size limits
- JMAP batch and result limits
- per-device Ping concurrency limits
- SQLite/Redis state schema migrations

