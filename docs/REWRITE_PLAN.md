# Rewrite Plan

## Current Findings

This project should be implemented as a JMAP-native synchronization gateway, with ActiveSync as one frontend. Rust is selected for M0+ because the workload is dominated by async HTTP, long-lived Ping/EventSource connections, JSON/JMAP batching, bounded binary parsing, and Docker deployment. Go is also viable, but Rust aligns with Stalwart's ecosystem and gives stronger parser/memory-safety guarantees. Python is too costly for long-lived high-concurrency sync without careful worker design. C++ adds avoidable memory-safety and maintenance risk.

Primary references inspected:

- `references/Z-Push`
- Z-Push PR #187 fetched as `references/Z-Push` branch `pr-187`
- `references/Z-Push-stalwart-jmap`
- `references/stalwart`
- Microsoft Learn Open Specifications for ActiveSync protocol families
- Current Stalwart JMAP documentation

Stalwart documentation says JMAP clients discover the endpoint through `/.well-known/jmap`, which returns endpoint URLs and session details; the API endpoint is served at `/jmap`. Stalwart push documentation says EventSource and PushSubscription are supported, and the EventSource URL is listed in the JMAP Session object.

## Z-Push Architecture Summary

Z-Push is a PHP request-dispatch framework around ActiveSync:

- `src/index.php` initializes config/logging/request parsing, authenticates, handles `OPTIONS`, then dispatches commands.
- `src/lib/request/request.php` parses ActiveSync query parameters, base64 compact query forms, Basic auth, device id/type, protocol version, policy key, and request limits.
- `src/lib/request/requestprocessor.php` selects command handlers and wires WBXML decoder/encoder streams.
- `src/lib/request/*` implements command behavior: `Sync`, `FolderSync`, `Ping`, `Provision`, `SendMail`, `MoveItems`, `ItemOperations`, etc.
- `src/lib/wbxml/*` centralizes ActiveSync WBXML code pages and streaming decode/encode.
- `src/lib/core/statemanager.php`, `syncparameters.php`, `synccollections.php`, and `devicemanager.php` track SyncKeys, device state, hierarchy state, policy keys, filters, windows, and Ping.
- `BackendDiff` supplies diff-style folder/message import/export APIs used by many backends.

Required behavior to preserve: HTTP headers for protocol negotiation, Basic auth challenge behavior, base64 query support, device-specific SyncKey semantics, FolderSync-before-Sync expectations, Ping timeout semantics, WBXML codepages, and command status codes.

Legacy behavior not to preserve internally: PHP stream architecture, backend-oriented folder/message abstractions, IMAP/CalDAV/CardDAV assumptions, opaque serialized state, and polling when JMAP push can wake Pings.

## PR #187 Findings

PR #187 adds:

- `src/backend/jmap/config.php`
- `src/backend/jmap/jmap.php`
- `src/backend/jmap/jmap_client.php`
- `src/backend/jmap/jmap_contacts.php`
- `src/backend/jmap/jmap_calendar.php`

It is a Z-Push `BackendDiff` backend. It fetches the JMAP Session resource, takes the mail primary account, and implements mail, contacts, and calendar through JMAP method calls. It uses `Mailbox/get`, `Mailbox/set`, `Email/query`, `Email/get`, `Email/import`, `Email/set`, `EmailSubmission/set`, `ContactCard/query/get/set`, `AddressBook/get/set`, `CalendarEvent/query/get/set`, and `Calendar/get/set`.

Important limitation: PR #187 implements Z-Push backend behavior, not a clean JMAP-native gateway. Push is implemented by polling query state in `ChangesSink()` every five seconds, which this project should replace with Stalwart EventSource/WebSocket state change notifications.

## Current Stalwart Capability Findings

Current Stalwart source exposes JMAP capability-driven Session generation in `crates/jmap/src/api/session.rs`. Account capabilities are permission-driven and include mail, submission, contacts, calendars, blob, file node, principals, websocket, and Stalwart-specific capabilities.

Source types show first-class collection/data types:

- Mail: `Mailbox`, `Email`, `Thread`, `Identity`, `EmailSubmission`, `Blob`
- Contacts: `AddressBook`, `ContactCard`
- Calendars: `CalendarEvent`, `CalendarEventNotification`
- Files: `FileNode`
- Push: EventSource URL in Session plus JMAP push subscriptions

Tasks should initially map to JSCalendar task/todo semantics where Stalwart exposes them through calendar data. Notes do not appear as a native JMAP Notes type; M0 architecture defines a `NoteStore` boundary, with likely initial implementation as JMAP Email objects in a dedicated Notes mailbox or possibly FileNode-backed notes after experimentation.

## Implementation Phases

M0 foundation:

- Rust scaffold with Axum/Tokio/Reqwest/Serde/Tracing.
- ActiveSync endpoint and `OPTIONS`.
- Autodiscover endpoint.
- JMAP Session discovery and capability projection.
- Bounded WBXML decoder/encoder module.
- Health, readiness, metrics.
- Dockerfile and Compose files.

M1 mail receive/sync:

- Implement canonical collections and mail objects.
- `FolderSync` from JMAP `Mailbox/get` plus capability folders for Contacts/Calendars/Tasks/Notes. Initial full hierarchy response is implemented for Mailbox, AddressBook, and Calendar.
- `Sync` initial and incremental receive using `Email/query`, `Email/get`, `Email/changes`, and `Email/queryChanges`.
- Read/unread, move, delete via `Email/set`.

M2 mail send:

- `SendMail`, `SmartReply`, `SmartForward`.
- Upload MIME to JMAP Blob, import or create Email, submit with `EmailSubmission/set`.

M3 push:

- Per-user EventSource worker.
- Map JMAP state changes to pending ActiveSync Ping waiters.
- Reconcile missed events with `/changes` and session state.

M4-M7:

- Contacts through `AddressBook` and `ContactCard`.
- Calendars and tasks through JSCalendar/JMAP calendar data.
- Notes through `NoteStore`, initially without leaking storage choice into ActiveSync handlers.

M8:

- Provision, Settings, Search, ResolveRecipients, ItemOperations, meeting edge cases, and client-specific compatibility.

## Immediate Status

M0 scaffold has begun. Current code authenticates ActiveSync requests by fetching the JMAP Session resource, projects capabilities, decodes WBXML with limits, exposes health/readiness/metrics, and returns `501` for unimplemented commands.

Git commits are currently blocked because the workspace contains a read-only mounted empty `.git` directory that cannot be replaced by `git init`.
