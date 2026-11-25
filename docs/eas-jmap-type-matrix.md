# MS-ASCMD data-class matrix

Field-by-field companion to [eas-jmap-command-matrix.md](eas-jmap-command-matrix.md)
and [eas-jmap-gap-analysis.md](eas-jmap-gap-analysis.md). Every WBXML token byte
cited below is transcribed directly from [MS-ASWBXML] v20250520 (fetched as a PDF
from `officeprotocoldoc.z19.web.core.windows.net`), section 2.1.2.1's per-codepage
tables — the same primary source used to scaffold the corresponding `pub mod`
blocks in `src/wbxml/eas.rs`. Current implementation status is read fresh from that
file and `src/activesync.rs` at commit `77d3077`, not carried over from memory.

**Verdict key:** ✅ Implemented · 🟡 Possible via JMAP, not built · ⛔ No JMAP
equivalent · ❓ Needs live verification.

## Calendar (MS-ASCAL, WBXML codepage 4)

The single most consequential finding of this pass: **this gateway's Calendar
codepage constants were flagged in their own source comment as "reconstructed from
memory at lower confidence... no second source to cross-check against."** They've
now been checked directly against the primary spec for the first time — **all 9 are
correct byte-for-byte** (`ALL_DAY_EVENT=0x06`, `BUSY_STATUS=0x0d`, `DTSTAMP=0x11`,
`END_TIME=0x12`, `LOCATION=0x17`, `SENSITIVITY=0x25`, `SUBJECT=0x26`,
`START_TIME=0x27`, `UID=0x28`). Confidence upgraded; the low-confidence comment in
`eas.rs` should be treated as resolved, not as an open risk, going forward.

`write_calendar_add()` (`activesync.rs:2010`) emits exactly those 9 fields, in the
order `DtStamp, StartTime, Subject, Location, EndTime, AllDayEvent`. Everything else
in the codepage is unused. Verified fresh by direct grep: **zero occurrences of
`SyncClientCommandKind` anywhere in `sync_calendar_collection()`** — read-only sync,
no recurrence handling, no attendee handling, confirmed by absence rather than
inferred.

| Field | Token | Min. version | Status | JMAP (JSCalendar / RFC 8984, superseded by jscalendarbis) mechanism | Notes |
|---|---|---|---|---|---|
| Timezone | 0x05 | All | ✅ (`deploy-2026-11-24-eas-calendar-timezone-year-77d3077`) | JSCalendar IANA `timeZone` → MS-ASDTYPE `TIME_ZONE_INFORMATION` | Emits the 172-byte structure as Base64, with the event-year recurring DST transition rules; fixed-offset zones use zero dates. Invalid or missing IANA zones are omitted. The local event year is derived from its UTC start and zone, with the current UTC year as fallback. |
| AllDayEvent | 0x06 | All | ✅ | JSCalendar `showWithoutTime` | |
| Attendees / Attendee / Email / Name | 0x07-0x0A | All | ✅ two-way (`deploy-2026-08-26l`) | JSCalendar `participants` and `replyTo` | Each participant carries its email, name, role, optional flag, and participation status; organizer derives from role `owner`. |
| Body / BodyTruncated | 0x0B-0x0C | 2.5 only (12.0+ uses AirSyncBase:Body, codepage 17) | ✅ description sync | JSCalendar `description` + `descriptionContentType` | AirSyncBase Body preferences select plain or HTML content; truncation is reported according to the requested preference. |
| BusyStatus | 0x0D | All | ✅ versioned | JSCalendar `freeBusyStatus` | Maps free/tentative/busy/out-of-office; WorkingElsewhere is emitted only where the EAS version supports it. |
| Categories / Category | 0x0E-0x0F | All | ✅ | JSCalendar `categories` | Values round-trip; clear and omission are distinguished on client edits. |
| DtStamp | 0x11 | All | ✅ | `CalendarEvent`'s `updated` or the JMAP response's own timestamp | |
| EndTime | 0x12 | All | ✅ | `start` + `duration`, or explicit `end` (jscalendarbis) | |
| Exception / Exceptions / Deleted / ExceptionStartTime | 0x13-0x16 | All (16.0+ uses AirSyncBase:InstanceId for occurrence identity) | ✅ deletes, subject, plain/HTML body, location, time, all-day, availability, categories, reminders, attendee roster, read-only attachments (16.0+) | JSCalendar `recurrenceOverrides` plus enclosure `links` | Deleted occurrences map to `{"excluded":true}`; supported fields map to per-instance overrides. Sensitivity cannot map to JSCalendar `privacy` because RFC 8984 prohibits that property in recurrence patches. Attachments are read from Calendars-account blobs; URL-only enclosures are not fetched. |
| Location | 0x17 | All (16.0+ uses AirSyncBase:Location, codepage 17) | ✅ versioned | JSCalendar `locations` (map; `locations["1"].name` for the simple case) | Uses codepage 4 through 14.1 and AirSyncBase:Location for 16.0/16.1. Instance-specific location names and coordinates use the same JMAP property in `recurrenceOverrides`. |
| MeetingStatus | 0x18 | All | ✅ derived | Participant roles, authenticated user's status, and cancellation state | Emits appointment/meeting/received/cancelled states from the JSCalendar participants and event state. |
| OrganizerEmail / OrganizerName | 0x19-0x1A | All | ✅ two-way | JSCalendar `participants` entry with role `owner` | Organizer is represented by the owner participant; attendee lists exclude the organizer. |
| **Recurrence** (0x1B, container) with **Type** (0x1C), **Until** (0x1D), **Occurrences** (0x1E), **Interval** (0x1F), **DayOfWeek** (0x20), **DayOfMonth** (0x21), **WeekOfMonth** (0x22), **MonthOfYear** (0x23) | 0x1B-0x23 | All | ✅ Common rules, excluded occurrences, subject/plain/HTML body/location/time/availability/category/reminder/attendee overrides, read-only attachment overrides (16.0+) | Stalwart singular JSCalendar `recurrenceRule`, `recurrenceOverrides`, and enclosure `links` | Maps daily, weekly, monthly, nth-weekday, and yearly rules both ways. EAS occurrence deletes map to JSCalendar exclusions; supported fields map to per-instance overrides. EAS 16.0/16.1 rule or series start/end changes clear existing overrides atomically. Exception attachments are read-only and require enclosure links with JMAP blob IDs; URL-only enclosures are not fetched. Sensitivity and non-representable patterns remain unsupported; attendee roster overrides are supported. |
| Reminder | 0x24 | All | ✅ | JSCalendar `alerts` | Maps a representable occurrence-specific display alert to EAS minutes; changing it preserves unrelated alerts. Default-alert mode and ambiguous multi-display alert shapes fail closed. |
| Sensitivity | 0x25 | All | ✅ base event; ⛔ occurrence exception | JSCalendar `privacy` (`public`/`private`/`secret`) | RFC 8984 forbids `privacy` patches inside recurrence overrides, so per-occurrence sensitivity changes fail closed. |
| Subject | 0x26 | All | ✅ | JSCalendar `title` | |
| StartTime | 0x27 | All | ✅ | JSCalendar `start` (+ `timeZone`) | |
| UID | 0x28 | Client requests only through 14.1; successful Sync Add response includes UID | ✅ response | JSCalendar `uid` | EAS 16.0/16.1 clients MUST NOT send this legacy field in a command request; the server returns UID in the successful Add response. |
| AttendeeStatus / AttendeeType | 0x29-0x2A | 12.0+ | ✅ | JSCalendar participant `participationStatus` / `roles` | Accepted, declined, tentative, needs-action, and optional attendee status/type map to EAS values. |
| DisallowNewTimeProposal / ResponseRequested / AppointmentReplyTime / ResponseType | 0x33-0x36 | 14.0+ | 🟡 partial; read paths and DisallowNewTimeProposal write deployed (`deploy-2026-11-24-eas-disallow-new-time-proposal-write-721f7f2`) | JSCalendar participant `expectReply` and `participationStatus`; retained iCalendar property | ResponseRequested maps to all invitees' `expectReply` when uniform; mixed values are omitted. ResponseType is derived from the authenticated user's status. AppointmentReplyTime maps from the authenticated attendee's unique `scheduleUpdated` timestamp, including occurrence overrides. DisallowNewTimeProposal merges `X-MICROSOFT-DISALLOW-COUNTER` on create/update, preserves other retained properties, and verifies persistence via readback; a mismatch fails the operation. Mocked create/update/readback regressions pass; live storage round-trip remains to be exercised. |
| CalendarType / IsLeapMonth | 0x37-0x38 | 14.0+ | ⛔ | N/A — non-Gregorian calendar systems | Out of scope; JSCalendar is Gregorian-only in its core spec. |
| FirstDayOfWeek | 0x39 | 14.1+ | ✅ Implemented when negotiated version supports it | JSCalendar `recurrenceRule.firstDayOfWeek` | Parsed/written for 14.1, 16.0, and 16.1; omitted from 14.0. A server-side non-Monday value that cannot be represented to an older client is rejected rather than silently changed. |
| OnlineMeetingConfLink / OnlineMeetingExternalLink | 0x3A-0x3B | 14.1+ | ✅ read-only (`deploy-2026-11-24-eas-online-meeting-uris-0f5d869`) | JSCalendar `virtualLocations` | SIP/SIPS locations map to `OnlineMeetingConfLink`; HTTP(S) locations map to `OnlineMeetingExternalLink`, including recurring overrides. EAS treats these response-only, so client writes fail closed. |
| ClientUid | 0x3C | 16.0+ | ✅ Calendar Sync Add input | Durable replay identity, scoped to device and collection | Optional, max 300 characters; used instead of transport ClientId to preserve retry idempotency when ClientId changes. Rejected on pre-16 versions and non-Add commands. Production advertises 14.0, so this path is prepared but not advertised to clients. |

## Contacts (MS-ASCNTC, WBXML codepage 1)

Contact fields are now handled in both directions. Reads are returned by
`sync_contacts_collection()` and ItemOperations; Sync Add/Change updates are
state-guarded `ContactCard/set` calls, and unsupported bodies fail closed.

The remaining ~40 fields in this codepage (now fully scaffolded in `eas.rs`) split
cleanly by how well JSContact (RFC 9553) covers them:

| Field group | Status | JSContact mechanism | Notes |
|---|---|---|---|
| Anniversary, Birthday | ✅ (`deploy-2026-11-22-eas-contact-dates-5f03d66`) | JSContact `anniversaries` (`wedding`/`birth`) | Strict `YYYYMMDD` values map to full PartialDates; partial updates preserve the other kind under `ifInState`. |
| AssistantName | ⛔ | No semantics-preserving standard JSContact property | The EAS assistant name cannot be retained without inventing a relationship or extension. |
| AssistantPhoneNumber | ✅ | JSContact `phones` with the `assistant` label | Labeled assistant phone entries round-trip independently of other phone kinds. |
| Body/BodySize/BodyTruncated | ✅ (AirSyncBase:Body) | JSContact `notes` | Plain-text notes map through the `eas_notes` note key. Body preferences control output and truncation metadata; unsupported HTML input is rejected to avoid lossy conversion. Legacy Contacts:Body in EAS 2.5 is not used because this gateway advertises 14.0. |
| Business2PhoneNumber, BusinessFaxNumber, Home2PhoneNumber | ✅ (`deploy-2026-11-22-eas-contact-secondary-phones-6c3d00d`, business fax `deploy-2026-11-22-eas-contact-business-fax-78425f9`) | JSContact `phones` with work/private contexts and `fax` feature | Secondary numbers and business fax remain separate entries; partial updates merge the existing phone map under `ifInState`. |
| HomeFaxNumber, PagerNumber | ✅ (`deploy-2026-11-22-eas-contact-fax-pager-9e7ab32`) | JSContact `phones` with `fax`/`pager` features and private context for home fax | Partial updates preserve other phone entries under `ifInState`. |
| CarPhoneNumber, RadioPhoneNumber | ✅ (`deploy-2026-11-22-eas-contact-car-radio-60e5eca`) | JSContact `phones[].label` (`car`, `radio`) | Custom phone labels retain the EAS-specific phone kind across reads and writes. |
| Business/Home/Other Address{City,Country,PostalCode,State,Street} (15 fields total) | ✅ (`deploy-2026-11-22-eas-contact-addresses-0e8a071`) | JSContact `addresses` with `components` and work/private/other contexts | All three EAS address groups map to JSContact components; partial updates merge existing components under `ifInState`, and empty tags remove individual components. |
| Categories/Category | ✅ (`deploy-2026-11-22-eas-contact-categories-79585fc`) | JSContact `keywords` | Values round-trip; absent field leaves existing keywords untouched and an empty field clears them. |
| Children/Child | ⛔ *(today)* / 🟡 | No direct JSContact property — closest is a `relatedTo` entry per child, if named at all | ❓ unsurveyed. |
| Department | ✅ (`deploy-2026-11-22-eas-contact-department-6708d25`) | JSContact `organizations[].units[]` | Maps to the last unit in the organization hierarchy; partial edits preserve company and other organization fields. |
| OfficeLocation | ✅ | JSContact `addresses` entry labeled `office location` | The full office-location string is retained separately from structured postal addresses. |
| MiddleName, Suffix | ✅ (`deploy-2026-11-22-eas-contact-name-parts-c3f4378`) | JSContact `name.components` (`given2`, `generation`) | Partial edits merge the existing components under a JMAP state guard; empty fields clear the selected component. |
| Spouse | ⛔ *(today)* / 🟡 | `relatedTo` entry with relation type `"spouse"` | |
| Title | ✅ (`deploy-2026-11-22-eas-contact-name-prefix-c48ee61c`) | JSContact `name.components` (`prefix`) | Partial edits preserve other name components under `ifInState`. `JobTitle` continues to map separately to JSContact `titles`. |
| WebPage | ✅ (`deploy-2026-11-22-eas-contact-web-page-d97a2d2`) | JSContact `links[].uri` | HTTP(S) website links are emitted as EAS `WebPage`; updates preserve unrelated links. |
| YomiCompanyName, YomiFirstName, YomiLastName | ✅ | JSContact phonetic `name.components` and organization `sortAs` | Phonetic name parts and company reading round-trip through the backend's JSContact fields. |
| Picture (`0x3c`) | ✅ two-way | JSContact `media` (kind `photo`) as a bounded data URI | Reads and writes photo bytes up to 5 MiB; media entries unrelated to the EAS picture key are preserved on updates. |
| Alias, WeightedRank | ⛔ | 14.0+ only fields — ❓ unsurveyed JMAP equivalents | Low priority; `WeightedRank` is Outlook's own contact-frequency scoring, unlikely to have a JMAP analogue at all. |

## Email (MS-ASEMAIL, WBXML codepage 2) and AirSyncBase (MS-ASAIRS, codepage 17)

**Not re-derived field-by-field this pass** — unlike Calendar/Contacts, these two
codepages already went through the multi-day live-pcap-verified debugging effort
documented in the git log this week (DateReceived format, Preview/Body/Attachment
field order, NativeBodyType, MIME BodyPreference Type=4, entity decoding). Their
current known-good subset (`DATE_RECEIVED`, `DISPLAY_TO`, `IMPORTANCE`,
`MESSAGE_CLASS`, `SUBJECT`, `READ`, `TO`, `CC`, `FROM` in codepage 2; `BODY`,
`DATA`, `ESTIMATED_DATA_SIZE`, `TRUNCATED`, `PREVIEW`, `NATIVE_BODY_TYPE`,
`CONTENT_TYPE`, plus `BODY_PREFERENCE`/`TYPE`/`TRUNCATION_SIZE` for requests, in
codepage 17) is considered materially complete for the fields this gateway actually
uses, verified against real device behavior rather than spec-reading alone — a
different, stronger kind of confidence than the Calendar/Contacts fields above ever
had before this pass. AirSyncBase `InstanceId` (0x2D) is additionally parsed and
written for recurrence exclusions as required by the 16.0/16.1 spec; this mapping
has regression coverage but still needs real-client verification. Codepage 22
(Email2) has one field actually wired up now:
`CONVERSATION_ID` (0x09) is sent for every message with a JMAP `threadId`, as a
deterministic fixed-16-byte value (see the gap-analysis doc's conversation-threading
row for the full story, including why a redo was needed and what's still unverified
device-side). The rest of the codepage (`ConversationIndex`, `IsDraft`, `Bcc`, etc.)
remains scaffolded token constants only, unused.

## Tasks (MS-ASTASK, WBXML codepage 9)

✅ **Done, two-way (`deploy-2026-08-26i`)**. The gap-analysis doc's leading
hypothesis — riding Tasks on top of `CalendarEvent`'s JSCalendar `Task`
type — is confirmed correct, live: Stalwart accepts `@type: "Task"` on
`CalendarEvent/set create` and round-trips `title`/`due`/`start`/`progress`/
`percentComplete`/`priority`/`description` (all read back via
`CalendarEvent/get`, not just inferred from a clean create response). There
is no separate Tasks capability URN and none is needed — `GatewayCapabilities::
tasks`'s existing `has(CALENDARS)` alias was correct all along, just
unverified until now. `CalendarEvent/query` has no server-side filter for
`@type` (`unsupportedFilter`, checked live both as `"type"` and `"@type"`),
so `tasks_in_calendar()` (`jmap/client.rs`) fetches the whole calendar and
filters to `@type == "Task"` client-side.

Implemented: `Subject`/`Complete`/`DueDate`/`UtcDueDate` (tokens 0x20/0x0a/
0x0c/0x0d) — read via `sync_tasks_collection()`, write via `write_task_command()`
(both `activesync.rs`), backed by `tasks_in_calendar()`/`save_task()`
(`jmap/client.rs`); deletes reuse `destroy_calendar_event()` directly since
Tasks and Events share the same underlying JMAP id space. Full Add/Change/
Delete lifecycle live-verified over the real WBXML wire protocol against
`eas-test.khuo.ng`: FolderSync correctly advertises a "Tasks" folder (type 7,
`eas_folder_type::TASK` — cross-checked against z-push's `zpushdefs.php`
`SYNC_FOLDER_TYPE_TASK`/`SYNC_FOLDER_TYPE_USER_TASK`, same bar as the existing
`NOTE` constant); a throwaway task's Add → Change (Complete=1) → confirmed via
direct JMAP that `progress` actually became `"completed"` server-side →
Delete over the wire → confirmed via direct JMAP the id came back `notFound`.

**Known gap:** `write_task_command()`'s field order is NOT device-verified.
Unlike Calendar/Contacts/Email, no reference implementation exists to check it
against — `docs/PR187_ANALYSIS.md` confirms Tasks was never attempted in the
z-push fork this project was ported from. MS-ASCMD's `ItemProperties` group
(which all `tasks:*` elements belong to, fetched fresh from
learn.microsoft.com for this pass) is an `xs:choice`, not an `xs:sequence`, so
unlike Email/Calendar's own `ItemProperties`-adjacent but still order-sensitive
fields, the wire spec doesn't mandate an order here at all — the chosen order
(Subject, Complete, DueDate, UtcDueDate) is a reasonable guess, not a verified
one. Needs a real device Tasks sync (e.g. iOS Reminders via the EAS account)
before being trusted, same standing caveat as the ConversationId redo.

The codepage's recurrence-field shape (`Type`/`Start`/`Until`/`Occurrences`/
`Interval`/`DayOfMonth`/`DayOfWeek`/`WeekOfMonth`/`MonthOfYear`/`CalendarType`/
`IsLeapMonth`/`FirstDayOfWeek`, tokens 0x10-0x26) is parsed and validated in
`eas.rs`. Task recurrence rules map through the shared JSCalendar
`recurrenceRule` conversion (`deploy-2026-11-24-eas-task-recurrence-a28032b`).
Local tests cover WBXML parse/write and JMAP Task-object conversion; a
Task-specific Stalwart create/get and a real-client sync remain unverified.
Per-instance event exclusions and subject/plain-text body changes are
supported. Sensitivity and other unsupported event fields fail closed; Tasks
have no per-instance exception container in MS-ASTASK. Non-Gregorian patterns
remain unsupported. `Regenerate=0` is accepted and omitted on response, which
means the protocol default; `Regenerate=1` is rejected because JSCalendar has
no matching regeneration behavior. A completed recurring Task requires a
nonzero `DeadOccur`, and the response includes `DeadOccur=1` for that case.

## Notes (MS-ASNOTE, WBXML codepage 23)

✅, two-way, already fully implemented via the Email-backed synthetic-message
workaround (`src/jmap/notes.rs`) — the most structurally interesting piece of this
whole gateway, see that file's own module doc for the full reasoning. Not
re-analyzed here; nothing new to add beyond what's already documented there and in
the gap-analysis doc.

## Folder hierarchy types (MS-ASCMD FolderHierarchy, WBXML codepage 7)

✅ for the folder **types this gateway actually emits**
(`eas_folder_type::{INBOX, DRAFTS, WASTEBASKET, SENTMAIL, APPOINTMENT, CONTACT,
NOTE, USER_MAIL, USER_APPOINTMENT, USER_CONTACT}` in `model.rs`) — not
independently re-verified against the spec's full FolderType enum this pass, but
these have been live-tested extensively (every folder in the real account syncs
and displays with the correct icon/behavior on-device). `Tasks`'s own folder type
value exists in the spec but is unused here, consistent with Tasks having no
backing implementation at all.
