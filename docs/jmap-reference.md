# JMAP reference

Working reference for the JMAP surface this gateway talks to on Stalwart.
Primary sources: IETF Datatracker, checked live this session for exact
RFC/draft numbers and status rather than assumed from memory.

## Core: RFC 8620 — The JSON Meta Application Protocol (JMAP)

Published 2019-07-18 ([datatracker](https://datatracker.ietf.org/doc/html/rfc8620)).
Defines the transport-agnostic core: the Session object (capability
discovery + account list, `src/jmap/session.rs` in this repo consumes
this), the request/response envelope (`using`/`methodCalls` →
`methodResponses`), the generic `Foo/get`, `Foo/set`, `Foo/query`,
`Foo/changes`, `Foo/queryChanges` method shapes every JMAP data type
follows, back-references between method calls in one request
(`resultReference`), push subscriptions, and out-of-band binary
upload/download (the `uploadUrl`/`downloadUrl` templates this gateway
uses directly — see `upload_blob()`/`download_blob()` in
`src/jmap/client.rs`).

This gateway's `JmapClient::api_call()` (`src/jmap/client.rs`) is a thin
wrapper around exactly this envelope; every capability URN it declares
(`capabilities.rs`) must be listed in `using` for methods needing it, per
spec.

## Mail: RFC 8621 — JMAP for Mail

Published 2019-07-18 alongside 8620
([datatracker](https://datatracker.ietf.org/doc/html/rfc8621)). Defines
the mail-specific object types this gateway's whole mail-sync path is
built on:

- **Mailbox** — folder-equivalent (`Mailbox/get` backs `FolderSync`, see
  `collections()` in `src/jmap/client.rs`).
- **Thread** — conversation grouping. **Not currently consumed** by this
  gateway (`Email2:ConversationId` was tried and reverted — commit
  `d199d37`, "caused a real client-side error, roll back" — worth
  revisiting with a live pcap check rather than re-guessing the shape).
- **Email** — the message object (`Email/get`/`Email/set`/`Email/query`),
  this gateway's core data source for mail AND (via the workaround in
  `src/jmap/notes.rs`) for Notes.
- **SearchSnippet** — highlighted search-result snippets. Not consumed
  (Search/Find are both unimplemented — see the EAS reference doc).
- **Identity** — the "from" identities available for sending. **Not
  currently consumed** — `send_email()` in `src/jmap/client.rs` doesn't
  look up an Identity before calling `EmailSubmission/set`; worth checking
  whether Stalwart requires/prefers an explicit `identityId` for correct
  outbound headers, rather than assuming the implicit default is always
  right.
- **EmailSubmission** — the actual "send this email" object.
  **Confirmed live and working**: `send_email()` uploads the composed
  MIME as a blob then calls `EmailSubmission/set` with that blob id — this
  is how SendMail/SmartReply/SmartForward all actually deliver mail today.
- **VacationResponse** — the JMAP object for out-of-office autoresponders.
  **Confirmed present in Stalwart's advertised capabilities** (see
  `src/jmap/notes.rs`'s module doc: a live session response was checked
  and listed `vacationresponse` alongside `core/mail/calendars/contacts/
  submission/sieve/blob/quota/filenode/principals/emailpush/webpush/
  websocket`). This gateway does **not** currently call
  `VacationResponse/get` or `VacationResponse/set` anywhere — the EAS
  `Settings>Oof` handler (`settings()` in `src/activesync.rs`) is a
  hand-rolled stub that always reports "disabled" and doesn't persist a
  Set. This is a real, currently-unclaimed gap: wiring Oof to
  `VacationResponse/get`+`/set` is very likely a small, high-value fix —
  see the gap-analysis doc's roadmap.

RFC 8621 also defines Mailbox roles (inbox/sent/drafts/trash/junk) as a
generic `role` string property — this gateway currently infers folder
type by matching mailbox `name`/`role` heuristically in `collections()`;
**not independently re-verified this session** whether it's reading
`role` per the spec's exact enumerated values or guessing off names —
worth a direct code check before relying on it for a new folder-type
mapping.

## Calendars: draft-ietf-jmap-calendars

**Not yet an RFC** — current version is `-28` (fetched live: "last updated
2026-08-13, expires 2027-02-14",
[datatracker](https://datatracker.ietf.org/doc/draft-ietf-jmap-calendars/)).
Defines the `urn:ietf:params:jmap:calendars` capability, `Calendar` and
`CalendarEvent` objects. This gateway's `capabilities.rs` declares exactly
this URN, and `calendar_events_in_calendar()`/`CalendarEvent/query`+`/get`
in `src/jmap/client.rs` are confirmed working live against a real created
event (per that function's own code comment). The event's actual JSON
shape is JSCalendar (see next section) — the draft defines the JMAP
*methods* (get/set/query) around JSCalendar objects, not a separate data
model of its own.

## Calendar data model: RFC 8984 (superseded) → draft-ietf-calext-jscalendarbis

RFC 8984 ("JSCalendar: A JSON Representation of Calendar Data",
[datatracker](https://datatracker.ietf.org/doc/html/rfc8984)) is the
originally-published version, but is now **obsoleted** by
`draft-ietf-calext-jscalendarbis` (JSCalendar 2.0, fetched live — status
confirmed as an active, not-yet-RFC draft superseding 8984). This
gateway's `CalendarEventObject` struct in `src/jmap/client.rs` is
commented as matching RFC 8984's shape, confirmed against Stalwart's live
`CalendarEvent/get` response — **not verified this session** whether
Stalwart has since moved to emitting the jscalendarbis shape instead (the
two are close but not guaranteed field-identical); if a future calendar
field addition doesn't parse, check this first before assuming a gateway
bug.

## Contacts data model: RFC 9553 — JSContact

Published 2024-05
([datatracker](https://datatracker.ietf.org/doc/rfc9553/)) — reached full
RFC status, unlike calendars/contacts-API which are still drafts. Defines
the JSON contact-card shape (`Card` object: `name`, `emails`, `phones`,
`organizations`, `titles`, etc.) This gateway's `ContactCardObject` in
`src/jmap/client.rs` is commented as confirmed live against a real created
card and matches this RFC's shape.

## Contacts API: draft-ietf-jmap-contacts

**Not yet an RFC** (fetched live: `draft-ietf-jmap-contacts-10` exists on
datatracker). Defines the `AddressBook` and `ContactCard` JMAP object
types and their `get`/`set`/`query` methods around the RFC 9553 JSContact
data model — this is the `ContactCard/query`+`/get` pair this gateway
calls in `contacts_in_address_book()`. **Contacts sync is read-only** in
this gateway today (per that function's own doc comment) — no
`ContactCard/set` call exists yet for two-way sync.

Note: there is a *separate*, related draft,
`draft-ietf-jmap-jscontact`, defining the JSContact data format itself
(now folded into / superseded by RFC 9553's publication — the naming
history here is genuinely confusing between "JMAP Contacts" the API draft
and "JSContact"/"JMAP JSContact" the data-format draft/RFC; don't conflate
them when searching for future contact-sync features).

## Sieve: draft-ietf-jmap-sieve

**Not yet an RFC** (fetched live: `draft-ietf-jmap-sieve-22` on
datatracker). Defines `urn:ietf:params:jmap:sieve`, `SieveScript`
get/set/query/validate methods for managing a user's Sieve filter scripts
via JMAP. Stalwart's advertised capability list (confirmed live per
`src/jmap/notes.rs`'s doc comment) includes `sieve`. **This gateway does
not currently touch Sieve at all** — flagged in the EAS reference doc's
Settings/Oof gap, but note that Oof/VacationResponse is its own JMAP
object (see above) and does NOT require going through Sieve directly;
Sieve access would only matter for exposing arbitrary mail-filtering rules
through EAS (which has no real EAS equivalent surface anyway — Outlook's
"Rules" concept isn't part of MS-ASCMD's synced object model). Low
priority.

## Other capabilities Stalwart advertises (confirmed live)

Per `src/jmap/notes.rs`'s module doc, a real Stalwart session response's
capability list is: `core, mail, calendars, contacts, submission,
vacationresponse, sieve, blob, quota, filenode, principals, emailpush,
webpush, websocket`. Notably **absent**: any Notes-shaped or Tasks-shaped
capability — confirming both are structural gaps at the JMAP level, not
gateway oversights:

- **Notes** — no ratified JMAP Notes extension exists at all (checked:
  no `draft-ietf-jmap-notes` or similar found this session). This
  gateway's `src/jmap/notes.rs` already solved this by encoding notes as
  synthetic Email objects in a dedicated `"Notes"` mailbox — see that
  file's extensive module doc for the exact mechanics (stable-id-via-
  keyword workaround, category-via-slugified-keyword workaround). Any
  future object type hitting this same "JMAP has nothing for it" wall
  should look at reusing this pattern before inventing a new one.
- **Tasks** — same story: no JMAP Tasks capability exists. This
  gateway's `GatewayCapabilities::tasks` field is currently just aliased
  to `has(CALENDARS)` (`src/jmap/capabilities.rs`) — **this is a stand-in,
  not a real signal**; nothing in the codebase actually syncs Tasks yet.
  The realistic path, following the Notes precedent, would be VTODO
  components inside a JMAP Calendar (JSCalendar's `Task` type is part of
  jscalendarbis, see above) if Stalwart exposes calendar tasks that way —
  **unverified this session, needs a live check against a real Stalwart
  Calendar/CalendarEvent response for any `@type: "Task"` entries** before
  assuming this route works, same discipline as everything else in this
  gateway's history.
- **quota, filenode, principals, emailpush, webpush, websocket** — no
  current EAS-side use in this gateway. `filenode`/`blob` back the
  attachment/blob download path already in use. `principals` is the most
  interesting unexplored one — see the EAS reference doc's Search/Find
  section for why (potential GAL-search backing).

## Verification discipline

Every "confirmed live" claim in this document traces to a comment already
in this gateway's source that was itself checked against an actual
Stalwart response, not to spec-reading alone — carrying forward the exact
lesson from this session's DateReceived/OofMessage bugs (reading a spec's
*structure* correctly is not the same as confirming its *live behavior*).
Anything marked **[unverified]** or "not verified this session" above is a
genuine gap in this document, not a rhetorical hedge — check it against a
live JMAP response before building on it.
