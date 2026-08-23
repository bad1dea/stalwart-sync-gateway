# JMAP Capabilities

The gateway discovers `STALWART_JMAP_URL`, normally `http://stalwart:8080/.well-known/jmap`, and uses the Session resource instead of hardcoding `/jmap`, upload, download, or EventSource endpoints.

Internal capability flags:

- `mail`: `urn:ietf:params:jmap:mail`
- `submission`: `urn:ietf:params:jmap:submission`
- `contacts`: `urn:ietf:params:jmap:contacts`
- `calendar`: `urn:ietf:params:jmap:calendars`
- `tasks`: currently derived from calendars until task-specific Stalwart behavior is verified.
- `notes`: currently derived from mail/files availability, behind `NoteStore`.
- `files`: blob or file-node availability.
- `push`: EventSource URL present.
- `websocket`: JMAP websocket capability.
- `event_source`: EventSource URL present.

Current Stalwart source generates Session resources in `crates/jmap/src/api/session.rs` and gates account capabilities by user permissions.

