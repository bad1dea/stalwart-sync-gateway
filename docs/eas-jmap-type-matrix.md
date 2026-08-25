# MS-ASCMD data-class matrix

Field-by-field companion to [eas-jmap-command-matrix.md](eas-jmap-command-matrix.md)
and [eas-jmap-gap-analysis.md](eas-jmap-gap-analysis.md). Every WBXML token byte
cited below is transcribed directly from [MS-ASWBXML] v20250520 (fetched as a PDF
from `officeprotocoldoc.z19.web.core.windows.net`), section 2.1.2.1's per-codepage
tables — the same primary source used to scaffold the corresponding `pub mod`
blocks in `src/wbxml/eas.rs`. Current implementation status is read fresh from that
file and `src/activesync.rs` at commit `360bf8a`, not carried over from memory.

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
| Timezone | 0x05 | All | ⛔ *(today)* / 🟡 | `start` is local time + a separate IANA `timeZone` property (or absent = UTC) | The gateway already does real IANA-timezone-aware UTC conversion for `start_utc`/`end_utc` (`local_to_utc_eas` in `jmap/client.rs`, DST-tested) — the *conversion logic* exists, it just never emits the MS-ASCAL `Timezone` element (a binary-encoded `TIME_ZONE_INFORMATION` struct, not a plain string) back to the device. |
| AllDayEvent | 0x06 | All | ✅ | JSCalendar `showWithoutTime` | |
| Attendees / Attendee / Email / Name | 0x07-0x0A | All | ⛔ *(today)* / 🟡 | JSCalendar `participants` (map of participant objects, each with `email`, `name`, `roles`, `participationStatus`) | Directly needed for MeetingResponse too (command matrix) — the same JSCalendar property backs both "who's invited" (this) and "did I accept" (MeetingResponse). |
| Body / BodyTruncated | 0x0B-0x0C | 2.5 only (12.0+ uses AirSyncBase:Body, codepage 17) | ⛔ *(today)* / 🟡 | JSCalendar `description` | Event description/notes — not currently synced at all. |
| BusyStatus | 0x0D | All | ✅ | JSCalendar `freeBusyStatus` (`free`/`busy`) — MS-ASCAL's richer `Free`/`Tentative`/`Busy`/`OutOfOffice`/`WorkingElsewhere` has no direct JSCalendar equivalent (JSCalendar's enum is binary) | Currently a static/passthrough value — not independently re-verified this pass whether it round-trips through JSCalendar's coarser enum correctly or is hardcoded. |
| Categories / Category | 0x0E-0x0F | All | ⛔ | JSCalendar `categories` (map) | Same shape as the Notes categories workaround already solved (`jmap/notes.rs`'s `notecat-*` keyword slugification) — but JSCalendar has a real native `categories` property, so `CalendarEvent/set` wouldn't need Notes' keyword-slugging trick at all. Easier than it looks by analogy to Notes. |
| DtStamp | 0x11 | All | ✅ | `CalendarEvent`'s `updated` or the JMAP response's own timestamp | |
| EndTime | 0x12 | All | ✅ | `start` + `duration`, or explicit `end` (jscalendarbis) | |
| Exception / Exceptions / Deleted / ExceptionStartTime | 0x13-0x16 | All (ExceptionStartTime capped at 14.1, dropped 16.0+ with no listed replacement) | ⛔ *(today)* / 🟡 | JSCalendar `recurrenceOverrides` (a map keyed by the overridden instance's original start time, values are patch objects — `null` value = that instance is deleted) | Recurring-event exceptions ("this Tuesday's standup is cancelled," "move Thursday's meeting to 3pm") — structurally different shape (EAS: a flat list of exception objects; JSCalendar: a patch-map) so this is real translation work, not a field rename. |
| Location | 0x17 | 2.5-14.1 (16.0+ uses AirSyncBase:Location, codepage 17) | ✅ | JSCalendar `locations` (map; `locations["1"].name` for the simple case) | Gateway only emits the codepage-4 form — fine for the versions it advertises (`12.0,12.1,14.0` — all within the 2.5-14.1 window), but would need the AirSyncBase form too if 16.x is ever re-enabled. |
| MeetingStatus | 0x18 | All | ⛔ | No direct single-field JSCalendar equivalent — inferred from participant roles/`replyTo` presence | MS-ASCAL's `MeetingStatus` (0=appointment, 1=meeting, 3=meeting received as attendee, 5/7=cancelled variants) doesn't map onto one JSCalendar property; would need to be derived from whether `participants` is populated and whose entry is "me." |
| OrganizerEmail / OrganizerName | 0x19-0x1A | All | ⛔ *(today)* / 🟡 | JSCalendar `participants` entry with role `"owner"`, or a distinct top-level organizer concept in some JSCalendar drafts | Same participants-map mechanism as Attendees above — organizer is just a participant with a specific role, not a separate object. |
| **Recurrence** (0x1B, container) with **Type** (0x1C), **Until** (0x1D), **Occurrences** (0x1E), **Interval** (0x1F), **DayOfWeek** (0x20), **DayOfMonth** (0x21), **WeekOfMonth** (0x22), **MonthOfYear** (0x23) | 0x1B-0x23 | All | ⛔ **CONFIRMED BLOCKED (2026-08-25)** | JSCalendar `recurrenceRules` — Stalwart doesn't implement it | **The field-by-field mapping this row used to describe as the roadmap's next step is moot for now.** Live-tested directly against the real Stalwart instance: `CalendarEvent/set create` with a `recurrenceRules` array is rejected outright with `notCreated: {type: "invalidProperties", properties: ["recurrenceRules"]}` in every shape tried (minimal `{"frequency":"weekly"}`, with `@type: "RecurrenceRule"`, with `interval`) — the property itself is unrecognized, not validated-and-rejected for its content. `CalendarEvent/get` with `recurrenceRules`/`recurrenceOverrides` explicitly requested on an existing event silently omits both, no error. Same class of finding as `participants` (see Attendees row below) — blocked on Stalwart, not a translation-design question. The MS-ASCAL↔JSCalendar mapping described in the old version of this row (Type↔frequency, DayOfWeek as a Sunday=1..Saturday=64 bitmask, Until/Occurrences↔until/count) is preserved here for whenever Stalwart adds real support, but there's nothing to build against today. |
| Reminder | 0x24 | All | ⛔ *(today)* / 🟡 | JSCalendar `alerts` (map of alert objects with a `trigger` — typically an `OffsetTrigger` with a signed `ISO 8601` duration relative to `start`) | MS-ASCAL's `Reminder` is a single integer (minutes before start); JSCalendar's `alerts` supports multiple richer alerts — a lossy-in-one-direction mapping (many alerts → one reminder) if ever round-tripped both ways. |
| Sensitivity | 0x25 | All | ✅ | JSCalendar `privacy` (`public`/`private`/`secret`) | |
| Subject | 0x26 | All | ✅ | JSCalendar `title` | |
| StartTime | 0x27 | All | ✅ | JSCalendar `start` (+ `timeZone`) | |
| UID | 0x28 | All | ✅ *(assumed — not independently re-verified this pass)* | JSCalendar `uid` | |
| AttendeeStatus / AttendeeType | 0x29-0x2A | 12.0+ | ⛔ *(today)* / 🟡 | JSCalendar participant `participationStatus` / `roles` | Part of the same participants-map mechanism as Attendees. |
| DisallowNewTimeProposal / ResponseRequested / AppointmentReplyTime / ResponseType | 0x33-0x36 | 14.0+ | ⛔ | ❓ unsurveyed | Meeting-negotiation metadata (whether the organizer allows counter-proposals, whether a response was requested/given and when). No obvious single JSCalendar property surveyed this pass. |
| CalendarType / IsLeapMonth | 0x37-0x38 | 14.0+ | ⛔ | N/A — non-Gregorian calendar systems | Out of scope; JSCalendar is Gregorian-only in its core spec. |
| FirstDayOfWeek | 0x39 | 14.1+ | ⛔ *(today, and version-gated above this gateway's cap)* | JSCalendar `recurrenceRules[].firstDayOfWeek` | Unreachable at the currently-advertised protocol cap regardless. |
| OnlineMeetingConfLink / OnlineMeetingExternalLink | 0x3A-0x3B | 14.1+ | ⛔ *(today, and version-gated above this gateway's cap)* | ❓ — possibly JSCalendar `links` or a `virtualLocations` property in some drafts | Video-call links. Same version-cap caveat as FirstDayOfWeek. |
| ClientUid | 0x3C | 16.0+ | ⛔ *(today, and version-gated above this gateway's cap)* | Client-generated dedup id, likely maps to nothing JMAP-side (client-local concern) | Same version-cap caveat. |

## Contacts (MS-ASCNTC, WBXML codepage 1)

The 12 tokens this gateway uses (`COMPANY_NAME` through `BUSINESS_PHONE_NUMBER`)
were all cross-checked the same way as Calendar's — **also confirmed correct
byte-for-byte**, no low-confidence flag existed for these to begin with.
`write_contact_add()` (`activesync.rs:1809`) emits exactly those 12 fields in order.
Verified fresh: zero `SyncClientCommandKind` matches in `sync_contacts_collection()`
— read-only, same as Calendar.

The remaining ~40 fields in this codepage (now fully scaffolded in `eas.rs`) split
cleanly by how well JSContact (RFC 9553) covers them:

| Field group | Status | JSContact mechanism | Notes |
|---|---|---|---|
| Anniversary, Birthday | ⛔ *(today)* / 🟡 | JSContact `anniversaries` (map, kind `"wedding"`/`"birthday"`) | Straightforward — one property per field. |
| AssistantName, AssistantPhoneNumber | ⛔ *(today)* / 🟡 | JSContact has no dedicated "assistant" property; would go in a `relatedTo` entry or a custom extension | ❓ — needs checking whether Stalwart's JSContact implementation exposes anything assistant-shaped at all. |
| Body/BodySize/BodyTruncated (2.5-only — 12.0+ uses AirSyncBase:Body) | ⛔ | JSContact `notes` | Free-text notes field — not currently synced. |
| Business2PhoneNumber, CarPhoneNumber, HomeFaxNumber, HomeaPhoneNumber, PagerNumber, RadioPhoneNumber | ⛔ *(today)* / 🟡 | JSContact `phones` (array with `contexts`/`features` tags — e.g. `features: ["fax"]`, `contexts: ["private"]`) | This gateway already does phone-context mapping for the 3 phone types it supports (Mobile/Home/Business, per the gap-analysis doc) — extending to the rest is the same pattern repeated, not new design. |
| Business/Home/Other Address{City,Country,PostalCode,State,Street} (15 fields total) | ⛔ *(today)* / 🟡 | JSContact `addresses` (map of address objects with `components: [{kind, value}]` — street/locality/region/postcode/country each a separate component) | Real structural mismatch: EAS flattens each address type into individually-named fields (`BusinessAddressCity`, `BusinessAddressStreet`, ...); JSContact nests them as `components` inside one address object per context. Assembly/disassembly work, not a rename, for each of the 3 address types (Business/Home/Other). |
| Categories/Category | ⛔ *(today)* / 🟡 | JSContact `categories` (map) — a real native property, unlike Notes' keyword-slug workaround | Same easier-than-Notes situation as Calendar's Categories field above. |
| Children/Child | ⛔ *(today)* / 🟡 | No direct JSContact property — closest is a `relatedTo` entry per child, if named at all | ❓ unsurveyed. |
| Department, OfficeLocation | ⛔ *(today)* / 🟡 | JSContact `organizations[].units`, and no dedicated office-location property surveyed | ❓ partially unsurveyed. |
| MiddleName, Suffix | ⛔ *(today)* / 🟡 | JSContact `name.components` (kind `"given2"` for middle name, `"credential"`/`"generation"` for suffix depending on which) | Straightforward once mapped. |
| Spouse | ⛔ *(today)* / 🟡 | `relatedTo` entry with relation type `"spouse"` | |
| Title | ⛔ *(today)* / 🟡 | JSContact `titles` (distinct from `JobTitle`, which is already synced) | |
| WebPage | ⛔ *(today)* / 🟡 | JSContact `links` | |
| YomiCompanyName, YomiFirstName, YomiLastName | ⛔ | JSContact has phonetic-name support (`name.components` with `phonetic` variants) in some draft versions — ❓ unverified whether Stalwart's implementation includes it | Japanese phonetic-reading fields; low priority for this account regardless of JMAP support. |
| Picture (14.0+... actually "All" per the table, `0x3c`) | ⛔ *(today)* / 🟡 | JSContact `media` (kind `"photo"`) + a blob reference | Same blob-download mechanism already used for Email/Notes attachments — infrastructure exists, just not wired to Contacts. |
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
had before this pass. Codepage 22 (Email2) has one field actually wired up now:
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
`IsLeapMonth`/`FirstDayOfWeek`, tokens 0x10-0x26) is scaffolded in `eas.rs` but
unused, same status as Calendar's own recurrence fields — see that row above:
confirmed blocked on Stalwart's side (`recurrenceRules` rejected outright),
not a translation problem to solve here.

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
