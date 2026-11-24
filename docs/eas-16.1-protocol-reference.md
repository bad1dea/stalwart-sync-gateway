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

1. **Autodiscover** — client resolves account config (out of scope for
   this gateway; iOS is pointed at `EAS_PUBLIC_URL` directly, no
   Autodiscover wiring — see `compose/zoidberg/services/stalwart-sync-gateway-test.yml`'s
   comment on why that's deliberate for the A/B test).
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
uses KQL translation for mailbox searches and Principal text queries for
directory results. The parser supports Boolean groups, size/received,
importance and boolean comparisons, property qualifiers, WORDS, ALL, ANY, and
NONE. Quoted phrase order is enforced locally because Stalwart's native text
index does not preserve it; proximity operators remain unavailable. Find is protocol-version
gated to 16.1 and is not advertised while production negotiates only 14.0.

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
exact-match preference, then substring matching. Certificate retrieval and
free/busy are not provided. `ValidateCert` validates supplied certificates
against configured trust roots and optional CRLs. These command handlers have
end-to-end regression coverage; neither implies S/MIME message signing.
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
calendar-folder target behavior is protocol-version gated. The path is
end-to-end covered in `src/push_tests.rs`. Real-client 16.1 certification
remains separate from handler coverage.

## WBXML codepages (MS-ASWBXML)

Each EAS XML namespace maps to a numeric WBXML codepage, listed
individually on Microsoft Learn (fetched a sample this session — Code
Page 0 = AirSync, Code Page 2 = Email, Code Page 4 = Calendar, Code Page
17 = AirSyncBase — matches this gateway's own `src/wbxml/eas.rs` constants
exactly for the pages checked). The intro/overview page itself does not
carry the full page-number table in one place (checked live — it's a
one-paragraph purpose statement, the table is spread across per-codepage
subpages), so the **full 0–24-ish page list with exact numbers for every
page (Notes, RightsManagement, FindP2P, etc.) is [unverified this
session]** — this gateway's own `src/wbxml/eas.rs` module (built up over
this whole debugging saga, cross-checked field-by-field against z-push's
PHP `wbxmldefs.php` table, which is itself sourced from this same MS-ASWBXML
spec) is the more reliable reference to consult first; treat it as
higher-confidence than re-deriving codepage numbers from scratch.

Known from this gateway's own verified history:
- Codepage 0 = AirSync, 2 = Email, 4 = Calendar, 5 = Move, 6 =
  GetItemEstimate, 7 = FolderHierarchy, 13 = Ping, 14 = Provision, 17 =
  AirSyncBase, 18 = Settings, 20 = ItemOperations, 21 = ComposeMail
  (SendMail/SmartReply/SmartForward) — all directly read from
  `src/wbxml/eas.rs`'s `pub const PAGE: u8 = N` lines this session.
- Notes' own codepage number was **not** independently re-verified this
  session (Notes sync is implemented via the Email-backed workaround in
  `src/jmap/notes.rs`, which reuses Email's own WBXML shape rather than a
  dedicated Notes codepage's fields) — if a native EAS Notes codepage
  (MS-ASNOTE-backed field set) is ever wanted instead of the Email-hack,
  that page number needs pulling from a fetched MS-ASWBXML subpage before
  use, same discipline as everything else in this doc.

## Protocol version gating (14.0 vs 12.x vs 16.x)

The single highest-leverage bug this whole session (commit `a0c71e1`):
this gateway was advertising `MS-ASProtocolVersions: 12.1,14.0,14.1,16.0,16.1`
while only actually implementing the ≤14.0 field shapes. iOS negotiates
the *highest* version the server advertises, so it was parsing 16.x-shaped
fields (e.g. Attachment's `ContentType`, gated ≥16.0 in the real z-push
reference) that this gateway never intended to send — silent WBXML parse
failures on the client side. Fixed by capping `SUPPORTED_PROTOCOLS` to
`"14.0"`. The current source still advertises only 14.0; although many
handlers have forced-version 16.1 tests, no 16.1 version bump should be made
without the full field-shape audit and real-device certification below.

**Implication for any future version bump** (e.g. targeting real 16.1 to
pick up native Notes support, RightsManagement, or other 16.x-only
fields): every single WBXML class this gateway touches needs the *same*
protocol-version-conditional field-presence/order audit z-push's PHP
`$mapping` arrays already encode, not just "does the codepage token
exist" — this was the exact class of bug (structural vs semantic
verification) called out explicitly this session. Don't bump
`SUPPORTED_PROTOCOLS` without redoing that audit per-field, and validate
every change against a live device request/response diff the way this
session's fixes were, not against spec-reading alone.

## Remaining verification items

- Full WBXML codepage number table (all ~24 pages) — pull from
  Microsoft Learn's per-codepage subpages, not re-derived.
- KQL proximity operators — Stalwart 0.16.27 does not expose token positions
  in its query results, so NEAR/ONEAR are not locally evaluated yet. Quoted
  phrase order is already exact-filtered in the gateway.
- Recurrence — Stalwart 0.16.27 rejects `recurrenceRules` on create and omits
  it on read; recheck after a backend upgrade.
- Real-client 16.1 negotiation and behavior, including Tasks field order and
  enabled Oof rendering. Production remains on 14.0 until that pass.
