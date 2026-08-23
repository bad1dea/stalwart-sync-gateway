# Architecture

The gateway has three boundaries:

- Protocol frontends: ActiveSync HTTP, WBXML, command semantics, Autodiscover.
- Canonical sync model: accounts, collections, mail, contacts, calendars, tasks, notes, blobs, state tokens, changes.
- JMAP service layer: Session discovery, capability discovery, method batching, upload/download, EventSource/WebSocket push.

ActiveSync handlers must not manipulate raw Stalwart response objects directly. They map EAS payloads to canonical requests. The JMAP layer maps canonical requests to JMAP methods.

M0 modules:

- `src/activesync.rs`: HTTP frontend shell.
- `src/autodiscover.rs`: MobileSync Autodiscover response.
- `src/jmap/session.rs`: Session resource model.
- `src/jmap/capabilities.rs`: capability projection.
- `src/jmap/client.rs`: authenticated Session fetch.
- `src/model.rs`: initial canonical model.
- `src/state.rs`: state store trait and memory scaffold.
- `src/wbxml/`: bounded WBXML implementation.

