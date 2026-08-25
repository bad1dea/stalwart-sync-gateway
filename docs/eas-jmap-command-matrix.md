# MS-ASCMD command matrix

Every command in [MS-ASCMD] v20250520 (28.0, the current released version — fetched
directly from the PDF at
`https://officeprotocoldoc.z19.web.core.windows.net/files/MS-ASCMD/[MS-ASCMD].pdf`,
not from memory or a cached summary), cross-referenced against this codebase read
fresh at commit `360bf8a`. This is the exhaustive companion to
[eas-jmap-gap-analysis.md](eas-jmap-gap-analysis.md), which stays the summary/roadmap
document — this one is the per-command reference to scan when picking up any single
item.

**Verdict key:** ✅ Implemented · 🟡 Possible via JMAP, not built · ⛔ No JMAP
equivalent · ❓ Needs live verification before committing to 🟡 or ⛔.

**Cross-cutting fact that changes several verdicts below:** this gateway currently
advertises `SUPPORTED_PROTOCOLS = "12.0,12.1,14.0"` (`src/activesync.rs`) — a
deliberate cap put in place this week after the whole over-advertisement
root-cause saga (commit `a0c71e1`). Any command or field gated to 14.1+ or 16.x is
**structurally unreachable from a real device today**, independent of whether it's
implemented, until that cap is deliberately revisited (its own decision, with its
own risk — not a prerequisite to implementing any single feature below).

| Command | Section | Min. version | Status | JMAP backing | Notes |
|---|---|---|---|---|---|
| Autodiscover | 2.2.1.1 | 12.0 | ✅ | N/A (own endpoint) | Handled outside the `Cmd=` dispatch entirely, at `POST /Autodiscover/Autodiscover.xml` — not in the switch statement this table otherwise describes. Not independently re-verified this pass. |
| Find | 2.2.1.2 | **16.1 only** (every tag in WBXML codepage 25 is 16.1-gated — confirmed in [MS-ASWBXML]'s own per-tag version column) | ⛔ *(today)* / 🟡 *(if 16.1 is ever re-enabled)* | `Email/query` (mailbox) — GAL half unclear, see Search below | Falsely advertised in `SUPPORTED_COMMANDS` (`activesync.rs:41`) despite having no handler — falls through to 501. Given the version gate, implementing this before deciding to re-advertise 16.1 is wasted effort; the version decision comes first. |
| FolderCreate | 2.2.1.3 | 2.5 (all versions) | ✅ *(mail only)* | `Mailbox/set` create | `folder_create()` (`activesync.rs`). Mail-only, per the existing note here — no create primitive for the heuristically-derived address-book/calendar listings; a `note_`/`ab_`/`cal_`-prefixed ParentId is rejected (Status 5) before ever calling JMAP. Live-verified: success, name-collision (Status 2), and bad-parent (Status 5) paths all confirmed against the real account. |
| FolderDelete | 2.2.1.4 | 2.5 | ✅ *(mail only)* | `Mailbox/set` destroy | `folder_delete()`. Same scoping as FolderCreate. Live-verified: success, protected-folder rejection (Status 3, confirmed against a real Inbox-destroy attempt), and not-found (Status 4) paths. |
| FolderSync | 2.2.1.5 | 2.5 | ✅ | `Mailbox/get`, `AddressBook`-listing, `Calendar`-listing | `folder_sync()` (`activesync.rs:2406`). Always full-resync-as-Adds on key "0", fixed key "1" after — not a real incremental folder diff, functionally fine since the folder set rarely changes. |
| FolderUpdate | 2.2.1.6 | 2.5 | ✅ *(mail only)* | `Mailbox/set` update (rename/reparent) | `folder_update()`. Same scoping as FolderCreate. Live-verified: rename+reparent success and not-found paths. |
| GetAttachment | 2.2.1.7 | 2.5 | ✅ | `download_blob()` | Handled as the one plain-GET command outside the `Cmd=` POST scheme (`activesync.rs:62`, dispatched before the WBXML body is even parsed). `AttachmentName` is a synthetic `blobId\|\|name` string this gateway itself issues — no server-side state needed to resolve it. |
| GetHierarchy | 2.2.1.8 | 2.5, legacy | ⛔ | N/A | **Not advertised in `SUPPORTED_COMMANDS` at all** — not even a 501 stub. Spec describes it as functionally superseded by FolderSync for any client that supports Sync-style folder hierarchy (email folders only, no sync state, no incremental updates) — genuinely low priority; FolderSync already covers everything GetHierarchy would for any modern client. |
| GetItemEstimate | 2.2.1.9 | 2.5 | ✅ | Counts from `Email/query`/`ContactCard/query`/`CalendarEvent/query` | `get_item_estimate()` (`activesync.rs:573`). `FolderType` field bug fixed this session (commit `1d34644`). |
| ItemOperations | 2.2.1.10 | 12.0 | ✅ *(mail only)* | `Email/get` + `download_blob()` | `item_operations()` (`activesync.rs:310`). Only `Store="Mailbox"` is handled — anything else (Notes/Contacts/Calendar item fetch, `DocumentLibrary` store) returns Status 2 cleanly (line 337-344), not a hang or 400. Real device confirmed this session to use the *Sync-embedded* `<Fetch>` instead for opening messages (a different mechanism, inside `Commands`, not this command) — this path exists for other-client compatibility, not the primary flow. |
| MeetingResponse | 2.2.1.11 | 2.5 (core fields) / 14.1+ (`InstanceId`) / 16.0-16.1 (`ProposedStartTime`/`ProposedEndTime`/`SendResponse`) | ⛔ *(today)* / 🟡 | ❓ — no JMAP scheduling-reply mechanism surveyed yet | Advertised, no handler, 501. High-value gap per the roadmap — calendar sync has zero interaction path currently. Needs its own JMAP research pass: does `CalendarEvent/set` on the participant's own `participants` entry (updating `participationStatus`) actually trigger the real iTIP reply email JMAP is supposed to send, or does Stalwart need something else? Unverified either way — flagged ❓, not 🟡, until checked live. |
| MoveItems | 2.2.1.12 | 2.5 | ✅ *(mail only)* | `Email/set` (`mailboxIds`) | `move_items()` (`activesync.rs:669`). No move primitive wired for contacts/calendar/notes — a Notes move would need `save_note`'s `mailbox_id` param threaded through, unverified whether `MoveItems` dispatches there at all (a direct code check, not yet done, would settle this in under a minute if it matters). |
| Ping | 2.2.1.13 | 2.5 | ✅ | N/A (gateway-local long-poll) | `ping()` (`activesync.rs:273`). Empirically rock-solid over the entire multi-day A/B test this week; deliberately not touched further even where the WBXML codepage 13 table looked mismatched against one read of a reference — see the standing note in the code near `wbxml::eas::ping`. |
| Provision | 2.2.1.14 | 2.5 | ✅ | N/A (gateway-local accept-everything policy) | `provision()` (`activesync.rs:628`). `PolicyStatus` bug fixed this session (commit `5a49f9e`). |
| ResolveRecipients | 2.2.1.15 | 2.5 (core) / 14.0+ (`Availability`/free-busy) / 14.1+ (`Picture`) | ⛔ *(today)* / 🟡 | `Principal`? — completely unsurveyed | Advertised, no handler, 501. Used to resolve recipients + free/busy + optionally S/MIME certs. The free/busy half might be backed by a JMAP calendar-availability query if Stalwart has one; genuinely unchecked. Low daily-driver priority (autocomplete-adjacent, not core mail flow) per the existing roadmap. |
| Search | 2.2.1.16 | 2.5 (core) / 12.0+ (query operators: `And`/`Or`/`EqualTo`/etc.) / 12.1+ (`UserName`/`Password` — document-library auth) / 14.0+ (`ConversationId`) / 14.1+ (`Picture`) | ⛔ *(today)* / 🟡 | `Email/query` full-text filter (mailbox); GAL half — see below | Advertised, no handler, 501. **This command backs BOTH mailbox search (WBXML codepage 15) and GAL/directory search (codepage 16, keyed by `Store>Name == "GAL"`)** — they're the same command with a different result shape depending on which store was searched, not two separate things. Mailbox-search half is plausibly cheap (`Email/query`'s `text` filter, unverified against Stalwart specifically). GAL half needs `Principal` (`urn:ietf:params:jmap:principals`) checked for free-text search support — the only capability in Stalwart's advertised list that looks like a plausible backing object, per the existing roadmap doc, still unverified. |
| SendMail | 2.2.1.17 | 2.5 (WBXML `ComposeMail`, codepage 21) / 14.0+ (raw-MIME "simplified" transport) | ✅ | `EmailSubmission/set` (blob-upload then submit) | `send_mail()` (`activesync.rs:415`). Handles both transports — detects which one by whether the body parses as WBXML at all. Confirmed live and working (mail actually delivered, this session). |
| Settings | 2.2.1.18 | 2.5 | ✅ *(partial — see type matrix for the Oof/Get/Set breakdown)* | `VacationResponse` (unused), static echo for `DeviceInformation`/`UserInformation` | `settings()` (`activesync.rs:482`). `PrimarySmtpAddress` bug (unrecognized field breaking the whole parse) fixed by removal this session. Oof is the single cheapest high-value item on the whole roadmap — see the gap-analysis doc. |
| SmartForward | 2.2.1.19 | 2.5 (WBXML) / 14.0+ (raw-MIME) | ✅ *(threading fidelity unverified)* | Same `EmailSubmission/set` path as SendMail | Same handler as SendMail (`command.eq_ignore_ascii_case("SmartForward")` dispatches to `send_mail()`). Sends successfully; whether In-Reply-To/References/threading headers are actually correct hasn't had the same live-pcap scrutiny as DateReceived/Oof — flagged unverified per the gap-analysis doc, not assumed correct. |
| SmartReply | 2.2.1.20 | 2.5 (WBXML) / 14.0+ (raw-MIME) | ✅ *(threading fidelity unverified)* | Same as SmartForward | Same story as SmartForward. |
| Sync | 2.2.1.21 | 2.5 | ✅ *(mail/notes/contacts/calendar all two-way as of `deploy-2026-08-25t`; calendar recurrence still out of scope)* | `Email`/`ContactCard`/`CalendarEvent` `/query`+`/get`+`/set`, the synthetic-message workaround for Notes | `sync_mail()`, `sync_contacts_collection()`, `sync_calendar_collection()`, `sync_notes_collection()` (all in `activesync.rs`). Contacts and Calendar were read-only as of the last pass (verified by grep: no `SyncClientCommandKind` match in either function) — both rewritten overnight to the same Add/Change/Delete + hash-diff pattern Notes already used, backed by `ContactCard/set`/`CalendarEvent/set`. Both confirmed live to support real in-place `update` (unlike `Email/set`), so neither needed Notes' stable-id-via-keyword workaround. Full Add→Change→Delete lifecycle live-verified for both with throwaway test items. Mail's own client-Add (e.g. append-to-Sent) is still explicitly ignored (`apply_mail_client_commands`, logged debug, no-op) — real EAS clients rarely Add mail directly since SendMail covers the compose path, low priority. |
| ValidateCert | 2.2.1.22 | 2.5 | ⛔ *(today)* / 🟡 | ❓ — no JMAP equivalent surveyed | Advertised, no handler, 501. S/MIME certificate-chain validation. Out of scope for a personal-account daily driver per the existing roadmap — flag to the user before investing regardless of verdict here. |

## What's advertised but not real

`SUPPORTED_COMMANDS` (`activesync.rs:41`) currently reads:

```
Sync,SendMail,SmartForward,SmartReply,GetAttachment,FolderSync,FolderCreate,
FolderDelete,FolderUpdate,MoveItems,GetItemEstimate,MeetingResponse,Search,
Settings,Ping,ItemOperations,Provision,ResolveRecipients,ValidateCert,Find
```

Real handlers exist for 14 of these (Sync, SendMail, SmartForward, SmartReply,
GetAttachment, FolderSync, FolderCreate, FolderDelete, FolderUpdate, MoveItems,
GetItemEstimate, Settings, Ping, ItemOperations). The other 5 (MeetingResponse,
Search, ResolveRecipients, ValidateCert, Find) fall through to a generic 501 —
meaning a real client that checks `MS-ASProtocolCommands` before attempting one of
these would see it listed as supported, then get a 501 when it actually tries.
Whether any real client does that pre-check (vs. just trying and handling the
error) is unverified; either way this is worth knowing before treating the
advertised list as documentation of actual capability.

## Legacy commands confirmed absent from the current spec

Checked the v28.0 table of contents directly (not assumed from an older memory of
the protocol): **`Notify`, `CreateCollection`, `DeleteCollection`, and
`MoveCollection`** — commands from early (pre-12.0) protocol versions — **do not
appear anywhere in this document at all**, not even as a deprecated/historical
section. They were apparently removed from the spec text entirely at some point
across its 28 major revisions, not merely marked obsolete. No action needed; noted
here so nobody goes looking for them.
