# EAS 16.1 protocol reference

Working reference for the Exchange ActiveSync (EAS) command surface this
gateway needs to speak. Primary sources: Microsoft Open Specifications,
fetched live for this document —
[MS-ASCMD](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-ascmd/)
(Command Reference Protocol, current revision 28.0, 2025-05-20) and
[MS-ASWBXML](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-aswbxml/)
(WBXML Algorithm). Where a claim below could not be pulled from a fetched
primary-source page in this session, it's marked **[unverified this
session]** rather than stated as fact — see the note at the bottom.

## Command workflow (per MS-ASCMD Overview, fetched live)

The spec's own overview lays out the intended command sequence:

1. **Autodiscover** — client resolves account config. The gateway implements
   Autodiscover and advertises the configured `EAS_PUBLIC_URL`. Real-client
   16.1 certification requires an isolated test endpoint; the previous
   `eas-test.khuo.ng` compose service is no longer present in the Homelab
   checkout (decommissioned in commit `6051729`).
2. **Provision** — device sends its info, gets + acks security policy.
3. **FolderSync** — retrieves the folder hierarchy.
4. **GetItemEstimate** — count of pending changes, then **Sync** to
   actually pull them (get a SyncKey, then items).
5. **Ping** or **Sync** — steady-state, keeps the device current.
6. **SendMail** / **SmartReply** / **SmartForward** — outgoing mail.
7. **ItemOperations** — fetch a message/attachment body; **MoveItems** —
   relocate items between folders.
8. **ResolveRecipients** / **ValidateCert** — S/MIME support.
9. **FolderCreate** / **FolderUpdate** / **FolderDelete** — folder
   mutation.
10. **MeetingResponse** — accept/decline/tentative a meeting request.
11. **Settings** — get/set server-side account parameters (Oof,
    DeviceInformation, UserInformation, etc.).
12. **Find** / **Search** — locate items (Find is the newer, GAL-search-
    capable command; Search is the older mailbox/GAL search command).

This gateway implements FolderSync, Sync, MoveItems, Provision,
GetItemEstimate, Settings, Ping, ItemOperations, SendMail/SmartForward/
SmartReply, GetAttachment, FolderCreate/Update/Delete, MeetingResponse,
Search, Find, ResolveRecipients, and ValidateCert — see
`src/activesync.rs`'s `post_handler`/`get_handler` dispatch. `Autodiscover`
is also implemented. This command inventory describes code presence; it does
not establish real-client 16.1 certification.

## Commands

### FolderSync (§2.2.1.5)
Retrieves/syncs the account's folder hierarchy in one shot (not
incremental per-folder like Sync — it's a single SyncKey covering the
whole hierarchy). Server returns folder Adds/Updates/Deletes since the
last key. This gateway's `folder_sync()` sends the full hierarchy on key "0",
then diffs collection fingerprints against per-device saved state and returns
only Adds, Updates, and Deletes. Unknown keys return Status 9; a device with
no saved hierarchy is safely baselined without a mass reset.

### FolderCreate / FolderUpdate / FolderDelete (§2.2.1.3/.6/.4)
Client-driven folder mutation (rename, move, delete, create). The gateway
supports mail folders through `Mailbox/set`, and user Contact/Calendar
folders through `AddressBook/set` and `Calendar/set`. Those groupware
collections are flat, so only root-level create/rename operations are valid.
Notes and Tasks are derived EAS folders and have no independent collection
lifecycle. FolderSync reports collection changes incrementally.

### Sync (§2.2.1.21)
The core per-collection incremental sync command — one `<Collection>` per
folder id, each with its own SyncKey, `<Commands>` (Add/Change/Delete/
Fetch from the client), and optional `<Options>` (BodyPreference,
Class, MimeSupport, MimeTruncation, etc.). This is where nearly all of
this session's bugs lived (field order/presence/date-format per the
`$mapping`-array structure z-push's PHP reference implements) — see the
commit history for the blow-by-blow. Real EAS clients (confirmed live,
this iPad) send a *different* BodyPreference per context: list-sync asks
Type=1 (plain) with a small TruncationSize; opening a message via a
Sync-embedded `<Fetch>` asks Type=4 (MIME) with MimeSupport=2 — this
gateway now (as of `deploy-2026-08-25o`) handles both.

### Ping (§2.2.1.13)
Long-poll heartbeat: client sends a HeartbeatInterval + a set of folders
to watch, server holds the connection until either a change occurs or the
interval elapses, then returns a status telling the client which folders
changed (triggering a follow-up Sync). This gateway's `ping()` has run for
the entire multi-day A/B test without a single observed failure — treat
its current token/shape as empirically solid even though a codepage-13
static-table read once appeared to disagree (see `src/activesync.rs`
around `ping()` for the standing note on why that was deliberately not
"fixed").

### ItemOperations (§2.2.1.10)
Generalized item-fetch command — can fetch a message body, an attachment,
or (with `Options>Schema`) a subset of fields, and supports fetching
multiple items across different stores in one request. This gateway accepts
multiple Fetch operations, applies each Store independently, parses body and
MIME options, and now restricts the returned Properties fields to the
top-level entries requested by Schema for mail, contacts, calendar events,
tasks, and notes. The real device this session uses the Sync-embedded
`<Fetch>` for opening messages instead (confirmed live), so this path remains
secondary for that client even though its protocol handling is broader.

### GetAttachment (§2.2.2.8, **legacy — pre-14.0 only**)
A plain `GET ...?Cmd=GetAttachment&AttachmentName=...`, no WBXML request
or response — the response body IS the attachment bytes. Superseded by
ItemOperations from 14.0 onward, but real Mail.app clients have been
observed (per this gateway's own code comments) still issuing it. This
gateway keeps it working via `get_handler()`, resolving
`AttachmentName` as a synthetic `"blobId||name"` reference it itself
handed out in `AirSyncBase:FileReference`.

### GetItemEstimate (§2.2.1.9)
Returns a count of pending changes per collection before the client does
a real Sync — lets the client show sync progress. This gateway's
`get_item_estimate()` implements this; FolderType was a real bug this
session (missing field, fixed in `deploy` commit `1d34644`).

### MoveItems (§2.2.1.12)
Moves one or more items between folders server-side. Implemented
(`move_items()`) — backed by JMAP's `Email/set` (updating `mailboxIds`).

### Search (§2.2.1.16) / Find (§2.2.1.2)
Search is the older mailbox-content + GAL search command; Find is its
newer replacement, notably capable of GAL (directory) lookups. Both are
implemented. Mailbox and GAL Search use JMAP Email/Principal queries; Find
uses KQL translation for mailbox searches and locally applies EAS's
prefix-string ANR match over paged Principal results for directory searches.
The parser supports Boolean groups, size/received,
importance and boolean comparisons, property qualifiers, WORDS, ALL, ANY, and
NONE. Quoted phrase order is enforced locally because Stalwart's native text
index does not preserve it. Direct `NEAR` and `ONEAR` operators now use exact
local token-distance matching over complete message text and common address
fields, including Boolean expressions with supported predicates. Trailing `*`
prefixes on proximity terms are matched against token starts. `attachment:`
name searches are matched locally against attachment metadata. String-property
`*` tests and negated property-presence restrictions are evaluated locally.
Text, JSON, XML, text-bearing PDF, EPUB XHTML, OOXML Word/Excel/PowerPoint
(including macro-enabled variants), OpenDocument Text/Spreadsheet/Presentation,
legacy `.doc`/`.xls`/`.ppt`/`.xlsb`, Outlook `.msg`, RFC 822 messages (including
nested attached messages), and allowlisted text/config/source files inside
generic ZIP archives are extracted for exact text/proximity matching. Word,
Excel, and PowerPoint package comments are included. Text attachments honor
declared charsets and XML declarations. Raster attachments in PNG, JPEG, GIF,
TIFF, BMP, and WebP use bounded English Tesseract OCR. PDFs use bounded
Poppler/Tesseract OCR for image-only pages: up to 100 such pages per attachment,
with a 30-second total runtime budget. Mixed PDFs retain native text and OCR
only pages without extractable text, regardless of total page count. Downloaded
attachments and extracted text are each capped at 16 MiB; generic ZIP expansion
is also capped at 16 MiB. Larger attachments and PDFs exceeding the OCR page or
time budget remain outside exact local matching.

GAL results currently map Stalwart `Principal` identity fields (display name,
alias, and email). When the request asks for photos, each result includes
`gal:Picture/Status=173` (`NoPicture`), since Principal records expose no photo
data; `MaxSize` and `MaxPictures` are validated against the protocol limits.
The optional `Options/Range` may be omitted. These paths have source-level
regression coverage; real-client 16.1 validation remains open. Find is
protocol-version gated to 16.1 and is not advertised while production negotiates
only 14.0. Mailbox results include the EAS `Importance` response property in
low/normal/high wire values.

### SendMail / SmartReply / SmartForward (§2.2.1.17/.20/.19)
SendMail posts a full new outgoing message; SmartReply/SmartForward use
the same transport but the client supplies the reply/forward MIME. From EAS 14.0
this uses the "simplified" transport: the POST body is raw RFC822 MIME
directly (not WBXML), with `SaveInSentItems`/`ClientId`/`ItemId`/
`CollectionId` as query params. This gateway implements all three
(`send_mail()`), backed by JMAP `EmailSubmission/set` (confirmed live —
see `src/jmap/client.rs`'s `send_email()`). A live self-addressed reply test
confirmed the submitted Message-ID, In-Reply-To, and References values were
preserved byte-for-byte in Sent Items and the delivered copy. The client
composes the quoted content; the gateway does not synthesize it from ItemId.

### ResolveRecipients / ValidateCert (§2.2.1.15/.22)
`ResolveRecipients` resolves principals and contacts with case-insensitive
exact-match preference, then substring matching. Certificate retrieval is not
provided. Availability is implemented for EAS 14.0+ using JMAP
`Principal/getAvailability`; Picture is implemented for 14.1/16.0/16.1 using
inline JSContact media or bounded ContactCard blob downloads, with the defined
MaxSize, per-query MaxPictures, and per-recipient status behavior.
`ValidateCert` validates supplied certificates against configured trust roots
and optional CRLs. These command handlers have end-to-end regression coverage;
neither implies S/MIME message signing.
See `src/activesync/resolve_recipients.rs` and
`src/activesync/validate_cert.rs`.

### Settings (§2.2.1.18)
Get/set server-side settings: DeviceInformation, UserInformation,
Oof (out-of-office), RightsManagementInformation. The gateway reads and
writes Oof through JMAP `VacationResponse`; DeviceInformation/UserInformation
are handled at the protocol edge. Enabled vacation behavior has not been
verified on a real client. RightsManagementInformation (IRM) remains out of
scope for this personal-account gateway.

### Provision (§2.2.1.14)
Device sends its info, gets security policy (PIN requirements, remote
wipe capability flags, etc.), then re-POSTs to acknowledge. This gateway
implements a minimal accept-everything policy (`provision()`) — the
PolicyStatus bug (SUCCESS vs NOPOLICY) was fixed this session
(`5a49f9e`).

### MeetingResponse (§2.2.1.11)
Accept/decline/tentatively-accept a meeting invite. The handler resolves an
Inbox invitation to its CalendarEvent and writes the participation response;
calendar-folder target behavior is protocol-version gated. Recurring series
responses update the master participant plus existing attendee overrides;
`InstanceId` on 14.1/16.0/16.1 changes only the selected occurrence. The updates
preserve other participant data and verify readback. Mocked regressions cover
series acceptance and occurrence decline. Invalid `InstanceId` formats return
status 104, and using an `InstanceId` with a non-recurring event returns status
146; regressions cover both. The current release is deployed as
`deploy-2026-11-25-eas-meeting-response-instance-status-dc6df90`; real-client 16.1
certification remains separate from handler coverage.

ResolveRecipients Availability (introduced in EAS 14.0) now uses JMAP
`Principal/getAvailability` for exact recipients and returns the required
30-minute MergedFreeBusy string. Input bounds and per-recipient errors are
covered by regressions. The implementation is deployed in
`deploy-2026-11-25-eas-resolve-recipient-availability-220f007`; live Stalwart
free/busy round-trip verification remains pending.

ResolveRecipients Picture is deployed in
`deploy-2026-11-25-eas-resolve-recipient-picture-per-query-18c02ea`. A mocked
gateway regression exercises a blob-backed ContactCard photo through the EAS
WBXML response, including protocol 14.0 rejection. Production still
advertises 14.0; this command implementation does not establish real-client
16.1 certification.

## WBXML codepages (MS-ASWBXML)

Microsoft's [MS-ASWBXML namespace table](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-aswbxml/2e57ccd4-cf38-45a0-bb11-95304bd1180d)
lists the complete mapping. The gateway's module constants match every
implemented page, checked together by
`codepage_numbers_match_the_ms_aswbxml_namespace_table` in
`src/wbxml/eas.rs`:

| Page | Namespace |
|---:|---|
| 0 | AirSync |
| 1 | Contacts |
| 2 | Email |
| 3 | AirNotify (no longer used; no module) |
| 4 | Calendar |
| 5 | Move |
| 6 | GetItemEstimate |
| 7 | FolderHierarchy |
| 8 | MeetingResponse |
| 9 | Tasks |
| 10 | ResolveRecipients |
| 11 | ValidateCert |
| 12 | Contacts2 |
| 13 | Ping |
| 14 | Provision |
| 15 | Search |
| 16 | GAL |
| 17 | AirSyncBase |
| 18 | Settings |
| 19 | DocumentLibrary |
| 20 | ItemOperations |
| 21 | ComposeMail |
| 22 | Email2 |
| 23 | Notes |
| 24 | RightsManagement |
| 25 | Find |

The Notes namespace has a confirmed codepage number, but this gateway uses
its Email-backed Notes workaround in `src/jmap/notes.rs`; it does not emit
native MS-ASNOTE fields.

## Protocol version gating (14.0 vs 12.x vs 16.x)

The single highest-leverage bug this whole session (commit `a0c71e1`):
this gateway was advertising `MS-ASProtocolVersions: 12.1,14.0,14.1,16.0,16.1`
while only actually implementing the ≤14.0 field shapes. iOS negotiates
the *highest* version the server advertises, so it was parsing 16.x-shaped
fields (e.g. Attachment's `ContentType`, gated ≥16.0 in the real z-push
reference) that this gateway never intended to send — silent WBXML parse
failures on the client side. Production defaults to advertising only 14.0.
For isolated testing, `EAS_MAX_PROTOCOL_VERSION=16.1` advertises the audited
version list through 16.1 and includes the Find command. Keep production at its
14.0 default until the full field-shape audit and real-device certification
below succeed.

**Implication for any future version bump** (e.g. targeting real 16.1 to
pick up native Notes support, RightsManagement, or other 16.x-only
fields): every single WBXML class this gateway touches needs the *same*
protocol-version-conditional field-presence/order audit z-push's PHP
`$mapping` arrays already encode, not just "does the codepage token
exist" — this was the exact class of bug (structural vs semantic
verification) called out explicitly this session. Don't bump
the advertised protocol ceiling without redoing that audit per-field, and
validate every change against a live device request/response diff the way this
session's fixes were, not against spec-reading alone.

## Remaining verification items

- Tasks — MS-ASTASK Appendix A defines `AllProps` as a repeating `xs:choice`,
  so the schema does not prescribe an element order. Sync emits the fields in
  its current writer order. Task Add requires `Complete`, which accepts only
  the specified 0/1 values. The isolated Stalwart E2E regression has passed
  recurring EAS Add, JMAP persistence, and EAS Fetch. Real-client recurrence
  behavior remains to be checked.
- KQL proximity — direct `NEAR`/`ONEAR` and Boolean combinations with supported
  predicates exact-filter full message text and common address fields, using
  the default distance of eight or an explicit distance. Supported attachment
  payloads are locally extracted for exact token-position matching up to 16 MiB
  input and extracted text. PDF OCR supports up to 100 image-only pages within
  its total runtime budget; unsupported formats and over-budget inputs remain
  outside exact local matching.
- Recurrence — common EAS event/task rules map to Stalwart's singular
  `recurrenceRule`; unsupported patterns fail closed. Task-specific backend
  create/get has passed the isolated E2E regression. Real-client recurrence
  behavior remains to be checked. Sensitivity cannot be patched per occurrence
  under RFC 8984.
- Real-client 16.1 negotiation and behavior, including Tasks recurrence and
  enabled Oof rendering. Set `EAS_MAX_PROTOCOL_VERSION=16.1` only on the
  isolated test bridge for that pass; production remains on 14.0 until it
  succeeds.
