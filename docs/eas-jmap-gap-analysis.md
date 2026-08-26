# EAS ↔ JMAP gap analysis and roadmap

Written by re-reading the actual Rust source on disk (not carried over
from any prior session's summary) at commit `c6beadf` (main,
2026-08-24/25), cross-referenced against
[eas-16.1-protocol-reference.md](eas-16.1-protocol-reference.md) and
[jmap-reference.md](jmap-reference.md). Goal: replace z-push (PHP) as the
daily-driver EAS gateway for one real account on one real iPad — mail
read/reply/send, plus calendar/contacts/notes sync.

## Status table

| EAS feature | Status | JMAP mechanism | Notes |
|---|---|---|---|
| Sync: Mail (read) | **Done** | `Email/query`+`/get` | Field order/format audited hard this session; MIME-open (Type=4) and list BodyPreference both handled. |
| Sync: Mail (mark read) | **Done** | `Email/set` (keywords `$seen`) | `apply_mail_client_commands`, Change command. |
| Sync: Mail (delete) | **Done** | `Email/set` destroy / `Email/destroy`-equivalent | Delete command, `destroy_email()`. |
| Sync: Mail (move) | **Done** | `Email/set` (`mailboxIds`) | Separate `MoveItems` command, not a Sync-embedded move. |
| Sync: Mail (client Add, e.g. append-to-Sent) | **Missing** | `Email/set` create + blob | Explicitly ignored (`apply_mail_client_commands`, logged debug, no-op). Real EAS clients rarely Add mail directly (SendMail covers the compose path) — low priority. |
| Sync: Mail (server-side delete surfaced to client) | **Done** (`deploy-2026-08-25v`) | Targeted `Email/get` (id + mailboxIds) per previously-seen-but-absent-from-window candidate | `emails_still_in_mailbox()` in `src/jmap/client.rs` — deliberately NOT a naive "absent from this fetch = deleted" diff, since `emails_in_mailbox`'s query is sorted newest-first and capped at the window size, so an old message pushed out of the window by newer mail looks identical to a real deletion under a plain diff. Live-verified both directions: a real destroyed message correctly surfaced as a Delete on the next Sync, and a real non-deleted message correctly confirmed present (not flagged) via the same JMAP call shape. |
| Sync: Contacts (read) | **Done** | `ContactCard/query`+`/get` (JSContact, RFC 9553) | Confirmed live against a real card. |
| Sync: Contacts (write: add/edit/delete) | **Done** (`deploy-2026-08-25s`) | `ContactCard/set` | `sync_contacts_collection` now handles Add/Change/Delete + a real hash-diff (same shape as Notes), backed by `save_contact`/`destroy_contact`. `ContactCard/set` confirmed live to support real in-place update, so the JMAP id is a stable ServerId with no workaround. Full lifecycle live-verified with a throwaway contact. |
| Sync: Calendar (read) | **Done** | `CalendarEvent/query`+`/get` (JSCalendar, draft-ietf-calext-jscalendarbis superseding RFC 8984) | Confirmed live against a real event. No recurrence handling confirmed — see below. |
| Sync: Calendar (write: add/edit/delete, non-recurring only) | **Done** (`deploy-2026-08-25t`) | `CalendarEvent/set` | Same treatment as Contacts, via `save_calendar_event`/`destroy_calendar_event`. `start`/`duration` written without `timeZone`, mirroring `local_to_utc_eas`'s existing "absent timeZone == already UTC" convention. Recurrence/attendees/reminders explicitly out of scope. Full lifecycle live-verified with a throwaway event. |
| Sync: Calendar recurrence (RRULE) | **Confirmed BLOCKED at the JMAP layer (2026-08-25)** | JSCalendar `recurrenceRules` — Stalwart rejects it outright | Live-tested directly, not just code-read: `CalendarEvent/set create` with `recurrenceRules` (multiple shapes tried — minimal `{"frequency":"weekly"}`, with `@type`, with `interval`) is rejected every time with `notCreated: {type: "invalidProperties", properties: ["recurrenceRules"]}` — the property itself is unrecognized, not just validated-and-rejected-for-content. `CalendarEvent/get` with `recurrenceRules`/`recurrenceOverrides` explicitly requested on an existing event silently omits both (no error, just absent) — same "silently drops the property" behavior already seen for `participants` (see MeetingResponse row). This is the same class of finding: blocked on Stalwart adding real recurrence support to `CalendarEvent`, not a gateway-side task. `write_calendar_command()` (renamed from `write_calendar_add` this session) still only emits the 6 non-recurring fields it always has; nothing to build here until Stalwart's JMAP support changes. |
| Sync: Tasks | **Done, two-way (`deploy-2026-08-26i`)** | `CalendarEvent` with `@type: "Task"` — confirmed live, NOT a separate JMAP object | Overturns the earlier "structurally unclear" verdict. Live-tested directly (2026-08-25): Stalwart accepts and round-trips `@type: "Task"` on `CalendarEvent` — `title`/`due`/`start`/`progress`/`percentComplete`/`priority`/`description` all confirmed by reading the object back, not just a clean create response. There's no separate Tasks-list capability or object; Tasks rides on the SAME Calendar storage as Events, distinguished only by `@type`, with no server-side query filter for it (`{"type"`/`"@type": "Task"}` both come back `unsupportedFilter`, checked live) — `tasks_in_calendar()` fetches the whole calendar and filters client-side. `GatewayCapabilities::tasks` was already `has(CALENDARS)` with a comment calling it "a placeholder, not a real signal" — that alias turns out to have been correct all along. Full Add/Change/Delete lifecycle live-verified over the REAL WBXML wire protocol (not just direct JMAP): FolderSync correctly advertises a "Tasks" folder (type 7); a throwaway task Add → Change (mark complete) → confirmed via direct JMAP read that `progress` actually became `"completed"` server-side → Delete over the wire → confirmed via direct JMAP that the id came back `notFound`. One real open gap: `write_task_command()`'s field order (Subject, Complete, DueDate, UtcDueDate) is NOT device-verified — unlike Calendar/Contacts/Email there's no reference implementation to check it against (this codebase's own `docs/PR187_ANALYSIS.md` says Tasks was never attempted in the z-push fork this project was ported from), and MS-ASCMD's `ItemProperties` group (fetched fresh from learn.microsoft.com) is an `xs:choice`, not an `xs:sequence`, so the wire spec itself doesn't mandate an order. Needs a live device Tasks sync (e.g. iOS Reminders via the EAS account) before trusting it, same standing caveat as the ConversationId redo. |
| Sync: Notes | **Done, two-way** | Email-backed synthetic-message workaround (`src/jmap/notes.rs`) | The most structurally interesting piece of this whole gateway — see that file's module doc. Full Add/Change/Delete supported. |
| FolderSync | **Done (simplified)** | `Mailbox/get`, `ContactCard`-AddressBook listing, `CalendarEvent`-Calendar listing | Always full-resync-as-Adds on key "0", fixed key "1" after — not a real incremental folder diff, but functionally fine for a folder set that rarely changes. |
| FolderCreate/Update/Delete | **Done** (`deploy-2026-08-25u`, mail only) | `Mailbox/set` create/update/destroy | `folder_create`/`folder_update`/`folder_delete` in `src/activesync.rs`. Mail-only per the original scoping note — a `note_`/`ab_`/`cal_`-prefixed id is rejected before calling JMAP. Error mapping (`alreadyExists`/`invalidProperties`/`forbidden`/`notFound`) confirmed live against the real account, including an actual rejected destroy-Inbox attempt. Full create→rename/reparent→delete lifecycle live-verified, including duplicate-name and bad-parent rejection paths. |
| Ping | **Done** | N/A (gateway-local long-poll + JMAP poll underneath, implementation detail not deeply re-read this pass) | Empirically rock-solid over the entire multi-day A/B test; deliberately not touched further this session even where a codepage table read once looked mismatched. |
| ItemOperations (Fetch) | **Done (legacy/secondary path)** | `Email/get` + `download_blob()` | Real device confirmed to use the *Sync-embedded* Fetch instead for opening messages; this command path exists for other-client compatibility. |
| ItemOperations (attachment fetch via ItemOperations, not GetAttachment) | **Confirmed NOT implemented -- corrects an earlier "presumably handles this" note** | `download_blob()` exists but isn't wired up here | Direct code read (`item_operations()`, `activesync.rs:320`): the only branch is `fetch.store.eq_ignore_ascii_case("Mailbox")`, which unconditionally treats the request as "fetch a whole email by ServerId" (`get_email_by_id` + `write_email_fields`) -- there's no attachment-specific path at all, no `download_blob()` call anywhere in this function. Moot in practice: the real device is already confirmed (this session and prior) to use the separate `GetAttachment` command for actual attachment downloads, which does correctly call `download_blob()`. Worth fixing only if a client that specifically needs ItemOperations-based attachment fetch shows up. |
| GetAttachment (legacy) | **Done** | `download_blob()` | `AttachmentName` reference is a synthetic `blobId||name` string this gateway itself issues. |
| GetItemEstimate | **Done** | Counts from the same `Email/query`/`ContactCard/query`/`CalendarEvent/query` paths used for Sync | FolderType field bug fixed this session. |
| MoveItems | **Done (mail only), non-mail behavior confirmed safe** | `Email/set` (`mailboxIds`) | Mail only. Confirmed by direct code read (no longer unverified): a Notes/Contacts/Calendar item would fail cleanly with Status 5 (`Email/set` rejects the non-Email id as `notFound`), not silent corruption. Currently moot anyway — every non-mail collection has exactly one folder, so no real client has anywhere to offer moving an item to. See command-matrix doc for the full reasoning. |
| Search / Find | **Missing** | Candidate: Stalwart's `Principal` object (`urn:ietf:params:jmap:principals`) for GAL/directory search; `Email/query` full-text filter for mailbox search | Both fall through to 501 despite `Find` being falsely advertised in `SUPPORTED_COMMANDS`. GAL search specifically needs checking whether Stalwart's Principal/query supports free-text search — unverified. |
| SendMail | **Done** | `EmailSubmission/set` (blob-upload then submit) | Confirmed live and working. |
| SmartReply / SmartForward | **Done, threading fidelity confirmed** | Same `EmailSubmission/set` path as SendMail | Live-tested this session with a self-addressed message carrying a real Message-ID/In-Reply-To/References triplet: byte-identical values confirmed on both the Sent Items copy and the delivered Inbox copy. See roadmap item 4 for the full test. |
| ResolveRecipients / ValidateCert (S/MIME) | **Missing, low priority** | No obvious JMAP equivalent surveyed | Out of scope for a personal-account iPad daily driver; flag to user before investing. |
| Settings: DeviceInformation / UserInformation | **Done (Get)** | Static/echo — no real JMAP-backed device-info storage | `PrimarySmtpAddress` bug (unrecognized field, cascading parse failure) fixed this session by removing it; `EmailAddresses>SMTPAddress` carries the same info in a valid shape. |
| Settings: Oof (Get) | **Done** *(disabled path live-verified; enabled path spec-derived, not device-verified)* | `VacationResponse/get` (`src/jmap/vacation.rs`) | Real account state now read on every Get. Disabled (`OofState=0`) shape confirmed live, unchanged from the earlier z-push comparison fix. Enabled shape's `OofMessage` block is built from MS-ASSETTINGS' own schema, not live-toggled -- see `src/jmap/vacation.rs` module doc for why (toggling `isEnabled=true` on the real account risks a genuine auto-reply going out unsupervised). Verify with a real device present before fully trusting the enabled path. |
| Settings: Oof (Set) | **Done, persists** | `VacationResponse/set` (`src/jmap/vacation.rs`) | Live-verified end to end with `isEnabled=false` (safe -- confirmed via direct JMAP query that `subject`/`textBody` actually persisted to the real `VacationResponse.singleton` object, then cleaned back up). `isEnabled=true` was deliberately never live-toggled for the same reason as the Get path above. |
| Settings: RightsManagementInformation (IRM) | **Missing, out of scope** | N/A | Enterprise feature, not relevant to this account. |
| Provision | **Done** | N/A (gateway-local, accept-everything policy) | PolicyStatus bug fixed this session. |
| MeetingResponse | **Still not implemented -- but reclassified: blocked on protocol version, not JMAP.** | MS-ASCMD 2.2.1.11 itself: *"When protocol versions 2.5, 12.0, 12.1, or 14.0 are used, MeetingResponse cannot be used to modify meeting requests in the Calendar folder"* -- only the Inbox meeting-request email. | **The earlier "blocked at the JMAP layer" verdict was WRONG and has been corrected (2026-08-25/26).** `participants` was never a Stalwart capability gap -- the original test omitted a required top-level `replyTo` (JSCalendar RFC 8984 §4.4.1, a companion property to `participants`+`expectReply`, found via a GitHub maintainer reply to an identical bug report) and, separately, each participant needs its own `calendarAddress` (found the same way: isolate one field, retest). Both fixed and live-verified (`deploy-2026-08-26l`) -- see the Attendees row below. What's ACTUALLY blocking MeetingResponse is unrelated: this gateway advertises protocol `12.0,12.1,14.0` (fixed, not touched), and the real spec text says at those versions MeetingResponse can only target the Inbox (the meeting-request email), never the Calendar folder directly. Building this needs the Inbox-side iTip flow understood first -- how Stalwart surfaces an incoming meeting request over JMAP, and how to correlate a MeetingResponse's Inbox `RequestId` back to the right `CalendarEvent` -- not yet investigated. |
| Sync: Calendar (attendees, read+write) | **Done, two-way (`deploy-2026-08-26l`)** | JSCalendar `participants` + top-level `replyTo`, each participant needs `calendarAddress` too | **Overturns the "blocked, same root cause as MeetingResponse" verdict above.** Write path: `extract_calendar_fields` accumulates repeated `calendar:Attendee` blocks into `CalendarFields.attendees`; `save_calendar_event` builds a `participants` map (organizer `role:owner` + one entry per invitee `role:attendee`[+`optional`]) plus the required `replyTo`, only when attendees is non-empty. Read path: `CalendarEventObject` requests `participants`, splits it into `organizer_email`/`organizer_name` (the `role:owner` entry -- confirmed against the live [MS-ASCAL] spec's own worked example that the organizer is NEVER listed as an Attendee) and `CalendarEvent.attendees` (everyone else); `is_organizer` is set by comparing `organizer_email` to the authenticated user. `write_calendar_command` emits OrganizerName/OrganizerEmail (PR #187 field order) and, when attendees is non-empty, MeetingStatus (1 organizer's copy / 3 received as attendee) plus Attendees (AttendeeStatus 0/2/3/4/5, AttendeeType 1/2/3 -- both confirmed against the live spec pages). Full round trip live-verified over the real WBXML wire protocol: a throwaway event with 2 attendees, AND the user's own real "Invite Calendar" event (organizer khuong@khuo.ng, khuong@khuong.info Accepted, khuong.hoang@outlook.com Tentative) both read back correctly with the right per-person AttendeeStatus codes. |
| Conversation threading (Email2:ConversationId) | **Done, device-confirmed (2026-08-26)** | JMAP `Email.threadId` -> deterministic fixed-16-byte value (UUID v5) | The `d199d37` revert's own root-cause hypothesis checked out on BOTH counts: (1) the original used WBXML token `0x0a` for `CONVERSATION_ID`, confirmed wrong against the primary MS-ASWBXML spec (fresh token-scaffolding pass, `e953835`) -- `0x0a` is actually `CONVERSATION_INDEX`, a different field; correct value is `0x09`. (2) the original sent the raw (often single-byte) JMAP `threadId` string directly; real Exchange servers send a fixed 16-byte GUID-shaped value, which iOS likely validates more strictly than the spec text implies. `eas_conversation_id()` now derives a deterministic fixed-16-byte value via UUID v5 (namespace + threadId). Field position corrected against z-push's own `syncmail.php` mapping too: the `>=14.0` block (which `ConversationId` belongs to) is appended strictly after the `>=12.0` block once version gates apply -- placed right after `NativeBodyType`. Verified: 52/52 unit tests pass; live against zoidberg, an 8-message sync produced 8 correctly-shaped 16-byte `ConversationId` values with zero WBXML corruption, and a second independent device sync of the same mailbox produced byte-for-byte identical values, confirming determinism live. **Device-confirmed 2026-08-26**: the user checked a real conversation on a real iPad against `eas-test.khuo.ng` -- grouped correctly, no sync errors. Still only deployed to the isolated zoidberg/eas-test instance; porting to production (hermes, currently still running the PHP z-push gateway) is a separate, much bigger cutover decision this confirmation doesn't itself authorize -- would need its own explicit go-ahead. |
| Attachment on send (composing/replying with a client-attached file) | **Done, confirmed live** | `upload_blob()` + `Email/import`, whole client MIME passed through opaque | Live-tested this session: a self-addressed `SmartForward` with a real `multipart/mixed` attachment part survived byte-identical (confirmed by downloading the blob and diffing content, not just checking metadata) on both the Sent Items and delivered Inbox copies. Root cause of why this "just works" with zero special handling: both EAS transports (raw-MIME 14.0+ and the WBXML-wrapped `ComposeMail` form) always require the CLIENT to submit the complete, already-composed MIME — attachments included — there is no "reference an existing attachment without re-uploading it" mechanism in the real protocol for this gateway to have missed. `rewrite_from_header()` only touches bytes before the header/body separator, so the entire multipart structure after it (including all attachment parts) was never at risk of mutation to begin with. |

## Roadmap, prioritized

### (a) Cheap — Stalwart's JMAP already exposes it directly

1. ✅ **DONE (`deploy-2026-08-25r`) — Wire Settings>Oof to
   `VacationResponse/get`/`/set`.** Shipped and live-verified overnight:
   Get now reads the real account state, Set actually persists (confirmed
   via a direct JMAP query showing `subject`/`textBody` landed on the
   real `VacationResponse.singleton` object). One deliberate gap left
   open: the `isEnabled=true` path was never live-toggled on the real
   account (real auto-reply risk, unsupervised) — the enabled Get
   response shape is spec-derived, not device-confirmed. Worth a live
   device test with the user present before fully trusting it. See
   `src/jmap/vacation.rs`'s module doc for the complete reasoning.
2. ✅ **DONE (`deploy-2026-08-25s` Contacts, `deploy-2026-08-25t`
   Calendar) — Contacts/Calendar two-way sync (`ContactCard/set` /
   `CalendarEvent/set`).** Shipped and live-verified overnight, full
   Add→Change→Delete lifecycle for both, using throwaway test
   items created via the real WBXML wire protocol (not directly via
   JMAP) and cleaned up after. One genuinely useful fact discovered
   along the way, worth knowing for any future Notes-style work: unlike
   `Email/set` (which can't update subject/body at all -- the reason
   Notes needs its whole stable-id-via-keyword workaround),
   `ContactCard/set` and `CalendarEvent/set` BOTH support real in-place
   `update` (confirmed live, not assumed) -- so the JMAP id itself is
   already a stable ActiveSync ServerId across edits for both classes,
   no workaround needed. Calendar recurrence/attendees/reminders remain
   explicitly out of scope (see the type-matrix doc for the real
   field-by-field recurrence mapping when that becomes the target).

2b. ✅ **DONE (`deploy-2026-08-25u`) — FolderCreate/FolderUpdate/FolderDelete,
    mail only.** Also shipped and live-verified this session, backed by
    `Mailbox/set`. See the status table row for the full story.
2c. ✅ **DONE (`deploy-2026-08-25v`) — server-side mail deletions surfaced
    as ActiveSync deletes.** Also shipped and live-verified this session.
    See the status table row — this one had a real correctness trap
    (window-vs-deletion ambiguity) that a naive port of the
    Contacts/Calendar/Notes pattern would have walked straight into.

### (b) High-value for the actual daily-driver use case

3. ✅ **DONE — Calendar attendees, two-way (`deploy-2026-08-26l`).**
   **This corrects TWO layers of wrong prior verdicts, not one.** The
   original read of `Calendar/get`'s `myRights.mayRSVP: true` as evidence
   of a working scheduling mechanism was already flagged wrong (it's a
   generic per-calendar ACL right). The FOLLOW-UP conclusion —
   "`participants` confirmed unsupported, blocked at the JMAP layer" —
   was ALSO wrong, and stayed wrong for a full extra session because
   nobody re-tested it after being told it was settled. What actually
   happened: the original test's `participants` payload was missing a
   required top-level `replyTo` (JSCalendar RFC 8984 §4.4.1 — found via a
   Stalwart maintainer's terse reply to an identical bug report on
   GitHub: *"Your event does not include an organiser"*), and, found
   separately by isolating one field at a time the same way, each
   participant also needs its own `calendarAddress`, not just `email` +
   `sendTo.imip`. Neither omission errors — both just cause a silent,
   total drop of the whole `participants` map on read-back, which is
   exactly why the earlier test read as "unsupported" instead of
   "malformed." Both fixed; participants round-trip perfectly now. See
   the Attendees row in the status table above for what got built and
   how it was verified (including against the user's own real invite).

   **What's still NOT done: MeetingResponse (Accept/Decline/Tentative).**
   This is a real, separate gap with a real, separate reason, found by
   actually reading the MS-ASCMD spec text rather than assuming it
   follows from the participants fix: *"When protocol versions 2.5,
   12.0, 12.1, or 14.0 are used, MeetingResponse cannot be used to modify
   meeting requests in the Calendar folder"* — only the Inbox
   meeting-request email. This gateway advertises `12.0,12.1,14.0`
   (fixed, not touched per the standing hard boundary), so a real device
   negotiating with it can only Accept/Decline through Mail, never by
   tapping a button directly on the Calendar event. Building this needs:
   (a) understanding how Stalwart's iTip pipeline surfaces an incoming
   meeting-request email over JMAP (does it auto-create a placeholder
   `CalendarEvent`? via `autoAddInvitations`? is there a stable
   correlation id between the email and the event?), and (b) mapping a
   MeetingResponse `Request`'s Inbox `CollectionId`+`RequestId` back to
   the right `CalendarEvent` to patch. Neither is investigated yet. Not
   guessable from here — needs its own session with a real inbound
   invite to test against.
4. ✅ **DONE (verification only, no code change needed) — SmartReply/
   SmartForward threading fidelity.** Live-tested this session: a
   self-addressed test message (no third party involved — sent
   khuong@khuo.ng to khuong@khuo.ng, safe by construction) with a real
   `Message-ID`/`In-Reply-To`/`References` triplet was submitted through
   the raw-MIME `SmartReply` transport, then read back via direct JMAP
   query from BOTH the Sent Items copy and the actually-delivered Inbox
   copy — all three header values were byte-identical to what was sent.
   Confirmed by direct source read first: `send_mail()` treats SendMail/
   SmartForward/SmartReply as one identical code path (the EAS-level
   distinction is purely client-side; the gateway does no ItemId/
   CollectionId-based original-message linking of its own), and the only
   MIME mutation in the whole pipeline is `rewrite_from_header()` in
   `src/jmap/client.rs`, which parses headers into logical (unfolded)
   lines, rewrites ONLY the `From:` entry, and leaves every other
   header's VALUE untouched (it does reformat other multi-line headers
   onto single unfolded lines when reassembling — semantically identical
   per RFC 5322, confirmed not to affect the live test's outcome). Test
   messages created and destroyed cleanly. No code change was needed —
   this item was a real open question with an unverified assumption
   underneath it, and it checked out as already correct.
5. ⛔ **CONFIRMED BLOCKED (2026-08-25) — Calendar recurrence.** Not a gap
   to build, a Stalwart limitation to wait on. Live-tested directly:
   `recurrenceRules` is rejected as `invalidProperties` on `CalendarEvent/set
   create` in every shape tried, and silently omitted from `CalendarEvent/get`
   even when explicitly requested — the property is entirely unimplemented on
   Stalwart's side, not merely unverified. Re-check if/when Stalwart's JMAP
   Calendars support changes; nothing for this gateway to build until then.
6. ✅ **DONE (`deploy-2026-08-26i`) — Tasks.** The "structurally hard, no
   direct JMAP equivalent" framing this item used to carry turned out to be
   wrong — moved out of section (c) entirely. Live-tested directly: Stalwart's
   `CalendarEvent` accepts and round-trips `@type: "Task"` objects (title/due/
   start/progress/percentComplete/priority/description all confirmed), so
   Tasks needed no Notes-style workaround at all — it rides on the same
   Calendar storage as Events. Full Add/Change/Delete lifecycle live-verified
   over the real WBXML wire protocol against `eas-test.khuo.ng`. See the
   status table row above for the complete story, including the one
   remaining gap (Task field order not yet device-confirmed).

### (c) Structurally hard — no direct JMAP equivalent, needs a workaround

7. **GAL/directory search (Search/Find).** `Principal`
   (`urn:ietf:params:jmap:principals`) is the only capability in
   Stalwart's advertised list that looks like a plausible backing object,
   but whether it supports the kind of free-text people-search GAL
   implies is completely unchecked. Likely a multi-session research +
   implementation effort, not a quick win — lowest priority of the
   concretely-scoped items above unless the user specifically wants
   directory search.

## What to verify before touching any of the above

Every item marked "Unverified" in the status table above should get a
direct source read (or a live JMAP/pcap check, per the item) before any
code changes — this document deliberately doesn't promote an unverified
claim to a design decision, per the standing lesson from this session's
DateReceived and OofMessage bugs: reading a spec or a comment's structure
correctly is not the same as confirming live behavior. Start with
whichever roadmap item the user actually wants next; don't build out (b)
or (c) items speculatively before (a)'s two items are done and verified
live, since they're both lower-risk and directly serve the same daily-
driver goal.
