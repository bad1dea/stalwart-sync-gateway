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
| Sync: Contacts (read) | **Done** | `ContactCard/query`+`/get` (JSContact, RFC 9553) | Confirmed live against a real card. |
| Sync: Contacts (write: add/edit/delete) | **Missing** | `ContactCard/set` | No client-command handling exists in `sync_contacts_collection` at all — verified by direct source read, no `SyncClientCommandKind` match anywhere in that function. Pure read mirror today. |
| Sync: Calendar (read) | **Done** | `CalendarEvent/query`+`/get` (JSCalendar, draft-ietf-calext-jscalendarbis superseding RFC 8984) | Confirmed live against a real event. No recurrence handling confirmed — see below. |
| Sync: Calendar (write: add/edit/delete) | **Missing** | `CalendarEvent/set` | Same story as Contacts — no client-command handling in `sync_calendar_collection`, verified by direct read. |
| Sync: Calendar recurrence (RRULE) | **Unverified/likely missing** | JSCalendar `recurrenceRules` | `write_calendar_add()` was reordered for field-order correctness this session but not audited for whether it reads/emits `RecurrenceType`/`Occurrences`/`Interval` etc. at all — needs a direct code check + a real recurring-event live test before claiming any status. |
| Sync: Tasks | **Missing, structurally unclear** | No JMAP Tasks capability exists at all (confirmed: Stalwart's advertised capability list has no tasks-shaped URN). `GatewayCapabilities::tasks` is currently just aliased to `has(CALENDARS)` — a placeholder, not a real signal. | Likely needs a JSCalendar `Task`-type-in-Calendar approach (unverified) or an Email-backed workaround like Notes. Needs its own live-verification pass before design. |
| Sync: Notes | **Done, two-way** | Email-backed synthetic-message workaround (`src/jmap/notes.rs`) | The most structurally interesting piece of this whole gateway — see that file's module doc. Full Add/Change/Delete supported. |
| FolderSync | **Done (simplified)** | `Mailbox/get`, `ContactCard`-AddressBook listing, `CalendarEvent`-Calendar listing | Always full-resync-as-Adds on key "0", fixed key "1" after — not a real incremental folder diff, but functionally fine for a folder set that rarely changes. |
| FolderCreate/Update/Delete | **Missing** | `Mailbox/set` (mail only — no create/rename primitive obviously exists for the address-book/calendar-listing paths, which are heuristic-derived, not stored objects) | Low priority — no observed daily-driver need for on-device folder management. |
| Ping | **Done** | N/A (gateway-local long-poll + JMAP poll underneath, implementation detail not deeply re-read this pass) | Empirically rock-solid over the entire multi-day A/B test; deliberately not touched further this session even where a codepage table read once looked mismatched. |
| ItemOperations (Fetch) | **Done (legacy/secondary path)** | `Email/get` + `download_blob()` | Real device confirmed to use the *Sync-embedded* Fetch instead for opening messages; this command path exists for other-client compatibility. |
| ItemOperations (attachment fetch via ItemOperations, not GetAttachment) | **Unverified** | `download_blob()` | `item_operations()` exists and presumably handles this — not independently re-verified in this pass which of GetAttachment vs ItemOperations the real device actually uses for attachments specifically (only messages were confirmed live this session). |
| GetAttachment (legacy) | **Done** | `download_blob()` | `AttachmentName` reference is a synthetic `blobId||name` string this gateway itself issues. |
| GetItemEstimate | **Done** | Counts from the same `Email/query`/`ContactCard/query`/`CalendarEvent/query` paths used for Sync | FolderType field bug fixed this session. |
| MoveItems | **Done** | `Email/set` (`mailboxIds`) | Mail only — no move primitive wired for contacts/calendar/notes (notes moves would need `save_note`'s mailbox param, unverified if MoveItems dispatches there). |
| Search / Find | **Missing** | Candidate: Stalwart's `Principal` object (`urn:ietf:params:jmap:principals`) for GAL/directory search; `Email/query` full-text filter for mailbox search | Both fall through to 501 despite `Find` being falsely advertised in `SUPPORTED_COMMANDS`. GAL search specifically needs checking whether Stalwart's Principal/query supports free-text search — unverified. |
| SendMail | **Done** | `EmailSubmission/set` (blob-upload then submit) | Confirmed live and working. |
| SmartReply / SmartForward | **Done, but threading fidelity unverified** | Same `EmailSubmission/set` path as SendMail | Whether In-Reply-To/References/threading is actually correct (vs. just "the mail sends") has not had the same live-pcap scrutiny DateReceived/Oof got — flagged as a real open question, not assumed correct. |
| ResolveRecipients / ValidateCert (S/MIME) | **Missing, low priority** | No obvious JMAP equivalent surveyed | Out of scope for a personal-account iPad daily driver; flag to user before investing. |
| Settings: DeviceInformation / UserInformation | **Done (Get)** | Static/echo — no real JMAP-backed device-info storage | `PrimarySmtpAddress` bug (unrecognized field, cascading parse failure) fixed this session by removing it; `EmailAddresses>SMTPAddress` carries the same info in a valid shape. |
| Settings: Oof (Get) | **Done (stub)** | **None currently** — hand-rolled, always reports disabled | Real, confirmed-available JMAP object (`VacationResponse`) is sitting unused. See roadmap below — this is probably the single cheapest high-value fix available. |
| Settings: Oof (Set) | **Stubbed, doesn't persist** | `VacationResponse/set` (not called) | Accepts the Set (no client-side hang/error) but silently drops it. |
| Settings: RightsManagementInformation (IRM) | **Missing, out of scope** | N/A | Enterprise feature, not relevant to this account. |
| Provision | **Done** | N/A (gateway-local, accept-everything policy) | PolicyStatus bug fixed this session. |
| MeetingResponse | **Missing** | Candidate: `CalendarEvent/set` (participant status update) or a dedicated JMAP scheduling reply mechanism — **not surveyed this session**, needs its own research pass | High-value gap for the calendar use case — see roadmap. |
| Conversation threading (Email2:ConversationId) | **Reverted, not active** | JMAP `Thread` object / `Email.threadId` | Tried once (commit `d199d37`), caused a real client-side error, rolled back without a live-diff root-cause the way other bugs got. Worth redoing properly with the pcap-comparison method now well-established in this project, rather than leaving it reverted indefinitely. |
| Attachment fetch on send (composing with an attachment) | **Unverified** | `upload_blob()` exists; whether `send_mail()`'s request-parsing path actually extracts and re-attaches client-supplied attachment blobs from a SmartReply/SmartForward's original message is unconfirmed | Needs a direct code read + live test with a real attached-file reply before claiming support either way. |

## Roadmap, prioritized

### (a) Cheap — Stalwart's JMAP already exposes it directly

1. **Wire Settings>Oof to `VacationResponse/get`/`/set`.** This is the
   standout item: Stalwart already advertises the `vacationresponse`
   capability (confirmed live), the current code is a stub with a comment
   explicitly saying real persistence is the intended next step, and the
   EAS-side shape (`Status/Get/OofState`, now correctly minimal per this
   session's fix) is already right — only the backing store needs to
   change from "always disabled" to a real `VacationResponse/get` call,
   and Set needs a real `VacationResponse/set` call. Low risk, directly
   fixes a feature the user already noticed was broken ("Automatic
   replies... loading").
2. **Contacts/Calendar two-way sync (`ContactCard/set` /
   `CalendarEvent/set`).** Both read paths are already solid and live-
   verified; the missing half is purely "handle
   Add/Change/Delete client commands the same way `sync_notes_collection`
   already does for Notes and `apply_mail_client_commands` does for
   Mail" — there's a working pattern to copy twice, not new design.

### (b) High-value for the actual daily-driver use case

3. **MeetingResponse (accept/decline/tentative).** Calendar sync is
   currently read-only in every sense, including the single most common
   calendar *interaction* on a phone — responding to an invite. This is
   probably the most user-visible gap once Contacts/Calendar sync (item 2)
   makes editing feel viable, and it needs its own research pass (JMAP
   side unsurveyed this session) before scoping.
4. **SmartReply/SmartForward threading-fidelity verification.** Mail send
   already works, but "does it actually thread correctly in the
   recipient's client" hasn't been checked with the same rigor as every
   other bug this project fixed. Given this project's own hard-won lesson
   (structural correctness ≠ live-verified correctness), this should get
   the same pcap-diff treatment before being trusted for real
   correspondence.
5. **Calendar recurrence.** A daily-driver calendar without recurring
   events (the majority of most people's actual calendar load — standing
   meetings, birthdays, etc.) is a soft-broken experience even though
   single events sync fine. Needs a direct code read of
   `write_calendar_add()`/the calendar-event JMAP mapping to establish
   current status before scoping the fix.

### (c) Structurally hard — no direct JMAP equivalent, needs a workaround

6. **Tasks.** The same class of problem Notes already solved (no native
   JMAP object) but with a materially less certain path forward — Notes
   had an obvious host object (Email, with a real existing "Notes"
   mailbox convention to reuse); Tasks has no equally obvious host.
   JSCalendar's `Task` type (part of jscalendarbis, not yet confirmed
   exposed by Stalwart's `CalendarEvent` objects) is the most promising
   lead but is explicitly unverified — this needs a live capability check
   before any design work, the same discipline every other claim in this
   document set was held to.
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
