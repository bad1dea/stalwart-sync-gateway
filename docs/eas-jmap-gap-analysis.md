# EAS ↔ JMAP gap analysis and roadmap

## Reconciled status for source `main` at `2f9cdbc` (2026-11-24)

The historical tables and roadmap below describe an August 2026 source snapshot;
they are not a current implementation inventory. The November source contains
draft create/edit with stable EAS identity, versioned flags and their metadata,
Find (mailbox and GAL), versioned calendar fields, multiple body preferences,
MIME policy, partial Sync/options persistence, and Sync long-poll/window
handling. Check the current Rust implementation before treating any older
“Missing” or “Not implemented” statement below as actionable.

Current prioritized work:

1. **Find/KQL parity.** The handler deliberately rejects query forms that
   Stalwart 0.16.19 indexes incorrectly, including multiword exact phrases.
   A fresh 0.16.27 probe confirms quoted `"alpha beta"` and `"beta alpha"`
   both match a message containing `alpha beta`; exact phrase semantics are
   still unavailable from the native index. KQL `importance:low|normal|high`
   now maps to the standard `Importance` header filter; invalid values and
   ordered/equality comparisons are rejected because JMAP header matching is
   text based. Mailbox Find also accepts `kind:email` as already satisfied by
   its Email-only query; other kinds remain unsupported in this scope.
   Received-date restrictions also support `tomorrow`, `last week`, `next
   month`, the `past month` alias, and weekday names for the current week;
   numeric `MM/DD/YYYY` and current-year `MM/DD` dates are also accepted.
   Relative weeks use Monday boundaries in UTC. ISO date-time restrictions
   with explicit offsets normalize to their UTC calendar day before
   constructing the filter. Extend coverage only where the backend preserves
   EAS meaning.
2. **Recurring series exceptions.** Common event and task recurrence rules
   now map to Stalwart's singular JSCalendar `recurrenceRule`. Per-instance
   exceptions (`recurrenceOverrides`) remain unsupported by the gateway, and
   exception edits are rejected until their EAS patch semantics are mapped
   and deployed Stalwart behavior is verified.
3. **Real-client 16.1 certification and rollout.** Production remains pinned
   to a tagged 14.0 build. Forced-version protocol tests do not establish that
   iOS/Outlook negotiate or behave correctly against an advertised 16.1 build.
   Keep 16.1 unadvertised until the implementation and a fresh device pass are
   complete.
4. **Legacy/low-priority commands and operations.** Mail Sync Add now creates
   drafts and non-draft Email items in the requested Mailbox. ResolveRecipients,
   ValidateCert, MeetingResponse, and ItemOperations fetches for mail, contacts,
   calendars, tasks, notes, and attachment blobs are implemented. Enterprise
   IRM remains out of scope for the personal-account gateway.
5. **Online meeting URI fields.** EAS defines `OnlineMeetingConfLink` and
   `OnlineMeetingExternalLink` from 14.1, but the deployed Stalwart 0.16.27
   CalendarEvent implementation has no `virtualLocations` property. The
   JSCalendar-standard mapping cannot be enabled until the backend exposes a
   representation.

Also completed (`b11914b`): **KQL timestamp offset normalization**. RFC3339
timestamps in `received:` restrictions now resolve to the UTC calendar day
before translation into JMAP date boundaries. Regression tests cover offsets
crossing a UTC day boundary in both directions. Full suite: 280 passed, 73
ignored; clippy with warnings denied, formatting, and diff checks pass.
Deployed as `deploy-2026-11-24-eas-kql-tz-b11914b`; homelab commit `163ac03`.
The forced Compose build produced image
`sha256:6d73b374107acbf6bc98e96dfbcf39b6b48500fcf82c7dcd761f11a3149466a4`.
The container started at `2026-11-24T05:39:57Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`. Production continues
to advertise EAS 14.0.

Also completed (`b981802`): **KQL message importance restriction**. Find now
maps `importance:low`, `importance:normal`/`medium`, and `importance:high` to
JMAP's standard `Importance` header filter. Unknown enum values and ordered or
equality comparisons remain rejected because the JMAP header matcher is
text-based. Regression tests check all three levels, case normalization,
`medium` normalization, and invalid/unsupported forms. Full suite: 276 passed,
73 ignored; clippy with warnings denied, formatting, and diff checks pass.
Deployed as `deploy-2026-11-23-eas-kql-importance-b981802`; homelab commit
`5dbd061`. Compose built image
`sha256:e2db7af99256433965e60e05d732d8b963b50222b14706d82dcd21e585f018a7`.
The replacement started at `2026-11-24T05:03:33Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed (`0b5503d`): **KQL email kind restriction**. Since mailbox Find
queries only JMAP Email objects, `kind:email` is satisfied by the mailbox
search scope and can be combined with other terms without changing the result
set. Other kinds stay unsupported because they cannot describe items in this
Email-only search. Regression tests check the no-op condition, composition
with another condition, and rejection of other kinds and comparisons. Full
suite: 277 passed, 73 ignored; clippy with warnings denied, formatting, and
diff checks pass. Deployed as `deploy-2026-11-24-eas-kql-kind-0b5503d`;
homelab commit `a9816d0`; runtime image
`sha256:861e8b95b6d7cbf8187d9b7286767747bb0f7ce67f44e471a42224a1c25c1dbc`
started at `2026-11-24T05:11:06Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. Production continues to
advertise EAS 14.0.

Also completed (`c960f05`): **KQL relative received dates**. In addition to
the existing today/yesterday/current-period support, the parser now handles
tomorrow, last week, next month, and the Exchange AQS `past month` term.
Relative week boundaries are Monday-based and relative date filters use UTC
because the request does not provide a client time zone. Regression tests use
a fixed date to cover all added ranges and their inclusive-day/exclusive-end
semantics. Full suite: 278 passed, 73 ignored; clippy with warnings denied,
formatting, and diff checks pass. Deployed as
`deploy-2026-11-24-eas-kql-relative-dates-c960f05`; homelab commit `321188b`.
Compose built image
`sha256:172c3bfb43aaf613377bbc66f29c037d52c0fc7dbd069ef48c50f11486f3c50a`.
The replacement started at `2026-11-24T05:17:50Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed (`d5de354`): **KQL received weekday restrictions**. English
weekday values now match that day within the current Monday-based week, with
case-insensitive parsing. Tests pin the date and verify Monday, Tuesday, and
Sunday boundaries. Full suite: 279 passed, 73 ignored; clippy with warnings
denied, formatting, and diff checks pass. Deployment as
`deploy-2026-11-24-eas-kql-weekdays-d5de354` completed through homelab commit
`43f2d15`. Compose built image
`sha256:aed78ce179af9be148752613cc420d3f0f55c7ad255fb28a93594f1af805ccb9`.
The replacement started at `2026-11-24T05:22:44Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed (`b90e06d`): **KQL numeric received dates**. In addition to
ISO dates and timestamps, the parser accepts Exchange-style `MM/DD/YYYY` and
`MM/DD` values (the latter uses the current year). Invalid month/day values
are rejected. Tests use a fixed date to cover both accepted formats and an
invalid date. Full suite: 280 passed, 73 ignored; clippy with warnings denied,
formatting, and diff checks pass. Deployment as
`deploy-2026-11-24-eas-kql-numeric-dates-b90e06d` completed through homelab
commit `1fb8700`. Compose built image
`sha256:915e541b180f0fdcb3b9ae5928a4c45573feb1d6302e5ab642289eccab911269`.
The replacement started at `2026-11-24T05:29:12Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Audit corrections from the current source: mail Sync Add creates idempotent
drafts and non-draft Email items with attachment/MIME support;
ResolveRecipients and ValidateCert have handlers and end-to-end tests;
MeetingResponse has an Inbox invitation path; ItemOperations Fetch supports
attachment FileReference and non-Mailbox object classes. The historical table
below still contains older claims that contradict these implementations and
should be read as historical evidence pending a full row-by-row rewrite.

Completed since this reconciliation began: **RTF body preference Type 3**.
Native plain/HTML content is now converted to escaped RTF, the body is marked
as type 3, and byte truncation plus AllOrNone are applied after conversion.
Coverage includes conversion escaping, Unicode control output, truncation,
AllOrNone, and an HTTP-level Sync Fetch. `cargo fmt`, clippy with warnings
denied, and the full suite passed (252 passed, 73 ignored). Production
was updated through the homelab GitOps pin `deploy-2026-11-22-eas-rtf-59ed916`
(homelab commit `3c40737`). The first Komodo pass did not rebuild the external
git context; verification caught the unchanged image, and the documented
forced Compose build then produced image
`sha256:f7dc7d00c3a52c34286017997860c6c659552e4206c854b14db1e221b6a9d7a0`.
The pre-deploy SQLite backup passed `PRAGMA quick_check`; the new container has
restart count zero, and mesh/public health plus public readiness returned 200.
No real-device RTF request was observed during rollout, so device rendering
remains unverified. Production protocol advertisement remains unchanged.

Also completed: **ValidateCert CheckCRL semantics**. `CheckCRL` now defaults
to false, accepts only one explicit `0`/`1` value, and controls whether an
unverifiable revocation status is returned as status 16. A certificate that
is verifiably revoked remains status 13 in either mode. Regression coverage
checks missing/invalid/duplicate values, missing/future CRLs, and revoked
certificates. The full suite passes (253 passed, 73 ignored), and
`cargo clippy --all-targets -- -D warnings` passes. This follows the MS-ASCMD
rule that the default is 0 and unverifiable revocation status should then be
ignored; strict checking must not ignore it.
Deployed as `deploy-2026-11-22-eas-checkcrl-a23c7f8`; homelab commit
`06ad1d0`. The forced Compose rebuild produced image
`sha256:28b76f2b4935e3260872baa7217db91833190ae96fce2b2cb86609e21cbbc3af`.
The new container started with restart count zero; mesh and public health and
readiness endpoints all returned successfully. Production continues to
advertise only EAS 14.0.

Also completed (`641d41f`): **Mail Sync Add to non-draft mailboxes**. Sync Add now writes
the requested target mailbox through `Email/set` or `Email/import`, keeps
idempotent replay markers, and omits the `$draft` keyword outside the Drafts
folder. A new regression test checks the non-draft mailbox properties; the full
suite passes (254 passed, 73 ignored), and clippy with warnings denied passes.
Production still advertises 14.0, so the Sync Add path remains version-gated to
16.0/16.1 until the planned client certification. Deployed as
`deploy-2026-11-22-eas-syncadd-641d41f`; homelab commit `a84981d`. The forced
Compose build produced image
`sha256:d7f5254079a94a0eb2e5a35419ba73a12a73053098b4d25303af1d1422f79cf3`.
The replacement started with restart count zero, and mesh/public health and
readiness all returned successfully.

Also completed (`7dd8863`): **KQL unknown-property fallback**. MS-KQL treats an
unrecognized property restriction as ordinary text tokens; the Find parser
now maps a simple `unknownproperty:word` restriction to text terms while
continuing to reject multiword quoted phrases whose order Stalwart does not
preserve. A direct 0.16.27 probe confirmed that native multiword text filters
require all terms, matching KQL's implicit AND behavior. Regression tests cover
unknown properties, comparisons, and phrase rejection. The full suite passes
(255 passed, 73 ignored), and clippy with warnings denied passes.
Deployed as `deploy-2026-11-22-eas-kql-7dd8863`; homelab commit `28cca02`.
Komodo reported success but retained the prior source ref, so runtime
verification caught the stale image. After fast-forwarding the Hermes checkout,
the forced Compose build compiled the pinned source and produced image
`sha256:cdb7435339fe776bdcf5242c56a24e1f2a678652c3817dbcbbc34aca6048d996`.
The replacement started at `2026-11-23T00:12:29Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. The gateway
continues to advertise only EAS 14.0.

Also completed in the current source: **EAS calendar recurrence mapping**.
The gateway parses and writes supported EAS Recurrence fields and translates
them to Stalwart's singular `recurrenceRule` property. A fresh, localhost-only
probe against Stalwart 0.16.27 verified create/get round-trip for a weekly
rule; the 1.2.0 probe confirmed the same wire shape. The original August
finding tested plural `recurrenceRules`, which is not Stalwart's property and
therefore did not establish a backend limitation. Per-instance exceptions,
unsupported calendar patterns, and real-client 16.1 certification remain
open. Regression tests: full suite 286 passed, 73 ignored; clippy with
warnings denied, formatting, and diff checks pass. Deployed as
`deploy-2026-11-24-eas-calendar-recurrence-bea1387`; homelab commit `8a26bfa`.
Image `sha256:69662846690fb009a6a8e918b384d55a8857c8a08677f10d3d0bf19192abb352`
started at `2026-11-24T06:11:06Z` with restart count zero; mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. Production continues to
advertise EAS 14.0; no client certification is claimed.

Also completed (`a28032b`): **ActiveSync Task recurrence mapping**. Task
Recurrence patterns now use the shared EAS↔JSCalendar conversion, including
the recurrence series start and version-gated `FirstDayOfWeek`. Empty
Recurrence clears a task series only for 16.0/16.1; unsupported shapes and
non-empty JMAP recurrence overrides fail closed. Regression suite: 290 passed,
73 ignored; clippy with warnings denied, formatting, and diff checks pass.
Deployed as `deploy-2026-11-24-eas-task-recurrence-a28032b`; homelab commit
`49fd351`. Image
`sha256:3bd2f1874c2719c5b39f89fa058696be289087c26ac22650e114adda47b8a618`
started at `2026-11-24T06:30:36Z`, restart count zero; mesh `/healthz` returned
`ok` and `/readyz` returned `ready`. The code uses CalendarEvent with `@type:
Task` and the same `recurrenceRule` property, but Task-specific backend create/
get and real-client recurrence paths have not been verified. Production
continues to advertise EAS 14.0.

Also completed (`2febaa0`): **EAS 16 Calendar ClientUid deduplication**.
Calendar Sync Add accepts the optional `ClientUid` for 16.0 and 16.1, validates
uniqueness and the 300-character limit, rejects it on older versions or
non-Add commands, and rejects the legacy request `UID` field for 16.0+. The
stable replay identity uses `ClientUid` when supplied, scoped to device and
collection, so retries remain idempotent if transport `ClientId` changes.
Without `ClientUid`, existing `ClientId` replay behavior is preserved.
Regression coverage checks parsing, legacy UID handling, and device-scoped
replay identity. Full suite: 292 passed, 73 ignored; clippy with warnings
denied, formatting, and diff checks pass. Deployed as
`deploy-2026-11-24-eas-calendar-client-uid-2febaa0`; homelab commit `7547f51`.
The forced Compose build produced image
`sha256:e599f552ada956f8b2fa2a282dd9555961de3203b7e5c91a95e91184efe76df9`.
The replacement started at `2026-11-24T06:44:02Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.
The protocol requirements are documented in Microsoft's [ClientUid]
(https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-ascal/d22282da-0cc5-4fba-b4b4-5aef341a6c8e)
and [UID](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-ascal/8f1fb00c-a156-49e8-9f54-478ecb0ce743)
specifications.

Also completed in the current source: **Contacts Sync Fetch**. A client Sync
`Fetch` command now returns the full contact fields in a Sync
`Responses/Fetch` entry, using the already-filtered address-book snapshot so a
contact in another book is not exposed. Missing or out-of-book IDs return EAS
status 8. Regression coverage checks success and collection scoping; the test
fixture now applies JMAP's `inAddressBook` query filter. Full suite: 293 passed,
73 ignored; clippy with warnings denied, formatting, and diff checks pass.
Deployed as `deploy-2026-11-24-eas-contacts-sync-fetch-5f3547a`; homelab
commit `249abda`. The forced Compose build produced image
`sha256:f64c7a9121683757d13c62475a28cb1ca65115528b8852b4f6bf6f0a0e3b2a3a`.
The replacement started at `2026-11-24T07:00:42Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed (`2f9cdbc`): **Calendar, Task, and Notes Sync Fetch**. These
collection handlers now return `Responses/Fetch` with the complete item data
and requested body preferences. IDs are resolved from the already-scoped
collection snapshot; missing or out-of-collection IDs receive status 8.
Regression coverage exercises all three classes, alongside the Contacts Fetch
success/scope test. Full suite: 294 passed, 73 ignored; clippy with warnings
denied, formatting, and diff checks pass. Deployed as
`deploy-2026-11-24-eas-groupware-sync-fetch-2f9cdbc`; homelab commit `a5f0f12`.
The forced Compose build produced image
`sha256:0f91868bc751cc8ee2a2e478dbc80b566e5b5ecdf06ad91667c8f86b70c8a76f`.
The replacement started at `2026-11-24T07:08:11Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed: **KQL property and qualifier compliance**. MS-KQL only recognizes a
property restriction when the property, operator, and value are contiguous;
`subject: mobile` is text search while `subject:mobile` is a property filter.
The parser now preserves this distinction. It also honors KQL's `+` property
qualifier (ignored) and `-` qualifier (NOT) for restrictions and terms. These
rules follow Microsoft's [property restriction](https://learn.microsoft.com/en-us/openspecs/sharepoint_protocols/ms-kql/1b4eb34a-0e20-4aca-b38e-41f531a747ff)
and [property qualification](https://learn.microsoft.com/en-us/openspecs/sharepoint_protocols/ms-kql/bc88a1fc-92ba-483a-98ec-5b6019cbf416)
specifications. Regression coverage checks spacing and qualified terms; the
Find integration fixture uses contiguous syntax for its property-search case.
Full suite: 256 passed, 73 ignored; clippy with warnings denied, formatting,
and diff checks pass. Exact phrase semantics remain limited by observed
Stalwart index behavior; recurrence was later implemented using its singular
`recurrenceRule` property. Real-client 16.1 certification still requires
device verification.
Deployed as `deploy-2026-11-23-eas-kql-spacing-dc0f3ce`; homelab commit
`6946218`. Komodo remained locked, so the Hermes checkout was fast-forwarded
and the pinned source was built through Compose. Image
`sha256:17faa133d0942f4ca59299d9f0060b662d9c490359291f45da58022eccd58247`
started at `2026-11-23T00:19:29Z` with restart count zero; mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. Production continues to advertise
EAS 14.0.

Also completed: **KQL boolean property comparisons**. `=` and `<>` now map
`isread`, `isflagged`, and `hasattachment(s)` to JMAP's exact boolean/keyword
filters; invalid values and ordered comparisons are rejected. Regression
tests cover true/false, yes/no, inequality inversion, and invalid inputs. Full
suite: 257 passed, 73 ignored; clippy with warnings denied, formatting, and
diff checks pass.
Deployed as `deploy-2026-11-23-eas-kql-bool-7e2b732`; homelab commit
`5b3232e`. Compose compiled the pinned source to image
`sha256:abc548e5a5d4132301200b390d745358b3e1e143d9b5e0aed55c1cb944e5caa6`.
The container started at `2026-11-23T00:30:45Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **KQL WORDS synonym queries**. `WORDS(a, b)` and its
whitespace-separated form now map to an OR of text filters, matching MS-SEARCH's
rule that an item matches when it contains one or more listed terms. Qualifiers
inside WORDS are ignored as specified. Property restrictions and multiword
phrases remain rejected inside WORDS because JMAP's text index cannot preserve
those semantics. Regression tests cover comma and whitespace forms, qualifiers,
and rejected inputs. Full suite: 258 passed, 73 ignored; clippy, formatting, and
diff checks pass. See [MS-SEARCH Synonyms](https://learn.microsoft.com/en-us/openspecs/sharepoint_protocols/ms-search/3e8338e0-d5fe-4a9f-ada9-85bc292d4c1a).
Deployed as `deploy-2026-11-23-eas-kql-words-3a5d8d7`; homelab commit
`e747269`. Compose produced image
`sha256:5817ae369361a17b9082a72f1db8ca1ae955768c584420beada3299117593308`.
The container started at `2026-11-23T00:36:57Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`.
Deployed as `deploy-2026-11-23-eas-kql-qualifiers-54731df`; homelab commit
`2256a11`. The Compose build compiled the pinned source and produced image
`sha256:1b17d36cc7a7c0d6958221280cbdfeb33536fa225d5033f44e8fb74b05dec009`.
The replacement started at `2026-11-23T00:25:18Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **KQL ALL, ANY, and NONE term operators**. These operators
map to AND, OR, and NOT(OR) filters, matching their set-membership behavior.
The implementation accepts text terms and rejects comma separators or
property/phrase operands the backend cannot preserve. Regression tests cover
each operator and rejected forms. Full suite: 259 passed, 73 ignored; clippy,
formatting, and diff checks pass. See the [MS-KQL operator specification](https://officeprotocoldoc.z19.web.core.windows.net/files/MS-KQL/%5BMS-KQL%5D-210720.pdf).
Deployed as `deploy-2026-11-23-eas-kql-set-ops-562d391`; homelab commit
`62b3f5a`. Compose produced image
`sha256:df88bc8a9bbca1a36467791b2bcfaf40a240b7d801872d18de14cc2a81fb1c53`.
The container started at `2026-11-23T00:44:03Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **FolderCreate/Update/Delete for Contact and Calendar
collections**. The existing mail-folder lifecycle remains hierarchical; new
Contact (`Type=14`) and Calendar (`Type=13`) folders map to `AddressBook/set`
and `Calendar/set` with synthetic `ab_`/`cal_` IDs. JMAP groupware folders
are root-level, and derived Notes/Tasks folders do not gain independent
CRUD. The mocked HTTP regression exercises create, rename, delete, invalid
types, and incremental FolderSync visibility for both types. Full suite: 260
passed, 73 ignored; clippy with warnings denied, formatting, and diff checks
pass. Deployed as
`deploy-2026-11-22-eas-groupware-folders-094b478`; homelab commit `b0c9a33`.
Compose rebuilt the pinned tag to image
`sha256:ce0a83b1553833ba027214488fae4a834aa820cdf7877fc0944c74c55d92dc18`,
started at `2026-11-23T00:59:39Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. The CRUD behavior is covered by
mocked HTTP regression tests; no live collection was created during deployment.

Also completed: **ActiveSync Contact categories**. EAS `Categories/Category`
values now round-trip through JSContact `keywords`. Updates preserve keywords
when the field is absent and clear them when an empty field is sent. Regression
coverage includes WBXML parsing/output and JMAP contact creation/read/write.
Full suite: 264 passed, 73 ignored; clippy with warnings denied, formatting,
and diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-categories-79585fc`; the runtime image digest,
`sha256:d0759fea361ad7cb08a9ff51fc486265db3cc4b58d236920c95b56dbdc129962`,
started at `2026-11-23T01:25:23Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. No live contact was modified
during deployment.

Also completed: **ActiveSync Contact middle names and suffixes**. `MiddleName`
and `Suffix` now map through JSContact `name.components` (`given2` and
`generation`) on create, read, update, and Sync output. Partial name edits
fetch the current name and merge only supplied components, preserving other
name parts under `ifInState`; empty tags remove the selected component.
Regression coverage checks WBXML parse/output, JMAP create/read, and a partial
update that changes the middle name while preserving first name, surname, and
suffix. Full suite: 265 passed, 73 ignored; clippy with warnings denied,
formatting, and diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-name-parts-c3f4378`; runtime image digest,
`sha256:354f288e6cc2f4ab7a9821310287868f7871365594c68008caf2f1b96ee37d4a`,
started at `2026-11-23T01:33:35Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. No live contact was modified
during deployment.

Also completed: **ActiveSync Contact birthdays and anniversaries**. EAS
`Birthday` and `Anniversary` values now map to JSContact `anniversaries` with
`birth` and `wedding` kinds. Strict `YYYYMMDD` validation prevents invalid
calendar dates; reads format valid full PartialDates back to EAS. Partial
updates preserve the other anniversary kind and use `ifInState`; empty fields
clear only the selected value. Regression tests cover date conversion,
WBXML parsing/output, create/read, partial-update preservation, and invalid
dates. Full suite: 266 passed, 73 ignored; clippy with warnings denied,
formatting, and diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-dates-5f03d66`; runtime verification is in
image `sha256:edb2cd9b347a59f343c0ecbcd871382690f34c5eb03e300d76b1b2e9fa31ff92`,
started at `2026-11-23T01:39:44Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. No live contact was modified
during deployment.

Also completed: **ActiveSync Contact name prefixes**. EAS `Title` maps to
JSContact `name.components` kind `prefix`, alongside given, middle, surname,
and generation components. Partial name changes merge the current components
under `ifInState`, so changing one name field retains the others. Regression
coverage checks WBXML parsing/output, JMAP create/read, and preservation during
a partial update. Full suite: 266 passed, 73 ignored; clippy with warnings
denied, formatting, and diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-name-prefix-c48ee61c`; runtime verification is
image `sha256:dfb72fc4be8d3c959604ecf5de94a8a788c48946cc32af393454dd172c4db005`,
started at `2026-11-23T01:48:13Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. No live contact was modified
during deployment.

Also completed (`f260a09`): **EAS Task completion timestamps**. Task Sync now
parses `Tasks:DateCompleted`, stores it in JSCalendar `progressUpdated`, reads
that property back for completed tasks, and emits it in WBXML. Incomplete-task
updates clear the completion timestamp; contradictory completion-date plus
`Complete=0` input is rejected as an item error. Regression coverage includes
WBXML decoding, UTC offset normalization, task Sync output, and a client Change
saved to the JMAP mock. Full suite: 269 passed, 73 ignored; clippy with
warnings denied, formatting, and diff checks pass. Deployed as
`deploy-2026-11-23-eas-task-completion-f260a09`; homelab commit `a7f5f62`.
Compose produced image
`sha256:bee5f6682599dff81913383e2bf3b21981d05742af656e42b3ae87e39559d369`.
The replacement started at `2026-11-24T04:17:59Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed (`6325c68`): **EAS Task sensitivity**. EAS Normal maps to
JSCalendar `privacy=public`; Personal and Private map to `private`; Confidential
maps to `secret`. Reading a private task returns EAS Private because JSCalendar
does not distinguish EAS Personal from Private. This follows the privacy
levels in [RFC 8984](https://www.rfc-editor.org/rfc/rfc8984.html) and
regression coverage checks all supported mappings, invalid EAS values, and a
client task Change. Full suite: 275 passed, 73 ignored; clippy with warnings
denied, formatting, and diff checks pass. Deployed as
`deploy-2026-11-23-eas-task-sensitivity-6325c68`; homelab commit `8e1b3df`.
Compose produced image
`sha256:976cd012343bd1d9c9f9963808e214637a87017ffba440e2d2934651154b1dca`.
The replacement started at `2026-11-24T04:55:49Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed (`ef6f5e9`): **EAS Task reminders**. `ReminderSet` and the
absolute UTC `ReminderTime` now map to JSCalendar display alerts. Updates retain
non-display alerts, clear the prior acknowledgement when changing the time,
and reject malformed input, multiple display alerts, or tasks inheriting
calendar-default alerts because those effective alerts cannot be safely
rewritten without expanding the calendar defaults. Regression coverage checks
WBXML validation, UTC conversion, alert preservation/clear, task Sync output,
and a client Change persisted through `CalendarEvent/set`. Full suite: 274
passed, 73 ignored; clippy with warnings denied, formatting, and diff checks
pass. Deployed as `deploy-2026-11-23-eas-task-reminder-ef6f5e9`; homelab
commit `6abddce`. Compose produced image
`sha256:07ad3f284ec192df4d043830a09c41ce3e09603dbaf1adec68ccd1c694f799e7`.
The replacement started at `2026-11-24T04:49:27Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed (`8955793`): **EAS Task categories**. `Tasks:Categories` now
maps to JSCalendar `keywords`; an omitted field preserves existing keywords,
and an empty category container clears them. Duplicate, empty, or malformed
category entries return an item error rather than being silently discarded.
Regression coverage checks parsing, JMAP read/write, and Sync output. Full
suite: 270 passed, 73 ignored; clippy with warnings denied, formatting, and
diff checks pass. Deployed as
`deploy-2026-11-23-eas-task-categories-8955793`; homelab commit `fafc043`.
Compose produced image
`sha256:19ac39e560616d90b8145dfde80ff9d0c50d7a21f7f53c7a643fe54933b111df`.
The replacement started at `2026-11-24T04:39:43Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed (`1f7f7c9`): **EAS Task importance**. EAS `Importance` values
0/1/2 map to JSCalendar priorities 9/5/1; reads map priority bands back to
low/normal/high, while an unspecified JSCalendar priority stays omitted.
Regression tests cover the full supported priority range, invalid EAS values,
WBXML output, and a client task Change. Full suite: 270 passed, 73 ignored;
clippy with warnings denied, formatting, and diff checks pass. Deployed as
`deploy-2026-11-23-eas-task-importance-1f7f7c9`; homelab commit `3318315`.
Compose produced image
`sha256:9331e35a119187f72aab8501aae0e94125521fd948637c65c6e068463cb7e6d2`.
The replacement started at `2026-11-24T04:33:17Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed (`4d00728`): **EAS Task start dates**. Tasks `StartDate` and
`UtcStartDate` now map to JSCalendar `start`, using the task's existing time
zone when present. Reads emit both local and UTC EAS fields; edits preserve
the existing start when the fields are omitted. Regression coverage checks
WBXML decoding/output and a client Change saved through `CalendarEvent/set`.
Full suite: 269 passed, 73 ignored; clippy with warnings denied, formatting,
and diff checks pass. Deployed as
`deploy-2026-11-23-eas-task-start-date-4d00728`; homelab commit `e9aef08`.
Compose produced image
`sha256:f3820a9c81b7777ece8f688ff00a7f4361c76a24156b04293849e7bd146d1873`.
The replacement started at `2026-11-24T04:26:06Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed: **ActiveSync Contact departments**. EAS `Department` maps to
the final JSContact `organizations[].units[]` entry. Updates merge into the
existing organization under `ifInState`, preserving its company name and other
organization data; an empty field removes the department. Regression coverage
checks WBXML parsing/output, create/read, and update preservation. Full suite:
266 passed, 73 ignored; clippy with warnings denied, formatting, and diff
checks pass. Deployed as
`deploy-2026-11-22-eas-contact-department-6708d25`; runtime verification is in
image `sha256:bf23b29f650629b02298c2ad23da259aa2ea9d52b6d3906095e0c88b80f3a60e`,
started at `2026-11-23T01:54:23Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. No live contact was modified
during deployment.

Also completed: **ActiveSync second business/home phone numbers**.
`Business2PhoneNumber` and `Home2PhoneNumber` now map to separate JSContact
phone entries with the same work/private contexts as their primary numbers.
Reads and Sync output preserve both values. Partial updates fetch and merge the
phone map under `ifInState`, selecting the matching contextual entry and
preserving other phone entries. Regression coverage checks WBXML parsing,
create/read/update, output, and hash changes. Full suite: 266 passed, 73
ignored; clippy with warnings denied, formatting, and diff checks pass.
Deployed as
`deploy-2026-11-22-eas-contact-secondary-phones-6c3d00d`; runtime verification
used the subsequent pinned build image
`sha256:bf23b29f650629b02298c2ad23da259aa2ea9d52b6d3906095e0c88b80f3a60e`,
started at `2026-11-23T01:54:23Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. No live contact was modified
during deployment.

Also completed: **ActiveSync home fax and pager numbers**. EAS `HomeFaxNumber`
and `PagerNumber` map to JSContact phone entries with `fax` and `pager`
features; the home fax also carries the private context. Reads, creates,
updates, and Sync output retain those field meanings while merging other phone
entries under `ifInState`. Regression coverage includes WBXML and JMAP
create/read/update plus output. Full suite: 266 passed, 73 ignored; clippy
with warnings denied, formatting, and diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-fax-pager-9e7ab32`; runtime verification is in
image `sha256:7546a12741a5933eb365d905ff43f068bd72df70093cdee03c632a76a1b86465`,
started at `2026-11-23T01:59:22Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. No live contact was modified
during deployment.

Also completed: **ActiveSync car and radio phone numbers**. `CarPhoneNumber`
and `RadioPhoneNumber` map to JSContact phone custom labels `car` and `radio`;
partial updates preserve the phone object and all other entries under
`ifInState`. The mapping uses JSContact's custom phone `label` property as
defined in [RFC 9553](https://www.rfc-editor.org/rfc/rfc9553.html). Regression
coverage checks WBXML parsing/output and JMAP create/read/update. Full suite:
266 passed, 73 ignored; clippy with warnings denied, formatting, and diff
checks pass. Deployed as
`deploy-2026-11-22-eas-contact-car-radio-60e5eca`; runtime verification is in
image `sha256:f196136aead624f6fb6e674c34dfb7bea3a2411e8ff0542b39860755e38c3b63`,
started at `2026-11-23T02:04:01Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. No live contact was modified
during deployment.

Also completed: **ActiveSync assistant phone number**. EAS
`AssistantPhoneNumber` maps to a JSContact phone entry labeled `assistant`.
Reads select that label; create and partial updates preserve other phone
entries and use the existing `ifInState` guard. Regression coverage checks
WBXML parsing/output, change hashing, and JMAP create/read/update. Full suite:
266 passed, 73 ignored; clippy with warnings denied, formatting, and diff
checks pass. Deployed as
`deploy-2026-11-22-eas-contact-assistant-phone-0a89e0a`; Hermes Compose built
image `sha256:4263f13c945bde8e8f03894e923fdd542fffa840d8454eb0a4282355d9e67834`.
The container started at `2026-11-23T02:20:47Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **ActiveSync phonetic first and last names**. `YomiFirstName`
and `YomiLastName` map to JSContact's `phonetic` value on the given-name and
surname components, with `phoneticScript: "Jpan"`; partial updates preserve
the base name and the other reading under `ifInState`. This follows the
phonetic component model in [RFC 9553](https://www.rfc-editor.org/rfc/rfc9553.html)
and the Japanese script subtag in the IANA language registry. Regression
coverage checks WBXML parsing/output, change hashing, and JMAP create/read/
update. Full suite: 266 passed, 73 ignored; clippy with warnings denied,
formatting, and diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-yomi-name-1a55270`; Hermes Compose built image
`sha256:d9adf8b27e02bb2528c7fc98118be9552b7f8daba3ba6ee3e9c9d5a22087fd77`.
The container started at `2026-11-23T02:30:56Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **ActiveSync phonetic company name**. `YomiCompanyName` maps to
the JSContact organization `sortAs` value, which carries the alternate
collation string while preserving the organization name and units on partial
updates. This follows the organization sorting property in
[RFC 9553](https://www.rfc-editor.org/rfc/rfc9553.html). Regression coverage
checks WBXML parsing/output, change hashing, and JMAP create/read/update. Full
suite: 266 passed, 73 ignored; clippy with warnings denied, formatting, and
diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-yomi-company-4860c47`; Hermes Compose built
image `sha256:257b69377ac1ee241448ff8d46844e71a0ab8a77c138a4bb5e1dbdcc26836c8c`.
The container started at `2026-11-23T02:36:42Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **ActiveSync contact photos**. The Contacts `Picture` value is
base64-decoded, validated against JPEG, PNG, GIF or WebP signatures, and stored
as a JSContact `media` photo using a data URI; reads convert supported data
URIs back to EAS base64. Updates replace or clear only the gateway-owned
`eas_picture` entry and retain other media. Inputs are limited to 5 MiB. This
uses the media resource shape in
[RFC 9553 section 2.6.4](https://www.rfc-editor.org/rfc/rfc9553.html#section-2.6.4).
Regression coverage checks WBXML parsing/output/hash changes, create/read/update,
format preservation and unrelated media retention. Full suite: 267 passed,
73 ignored; clippy with warnings denied, formatting and diff checks pass.
Deployed as `deploy-2026-11-22-eas-contact-photo-fe0962d`; Hermes Compose built
image `sha256:6ddde2c84b1a6ab45a78a9b3bd1c8a53f01db78b3368fa5cf9bc583568ce6230`.
The container started at `2026-11-23T03:34:12Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **EAS 16.x MeetingResponse SendResponse**. For protocol
16.0/16.1, an empty `SendResponse` now requests JMAP scheduling messages;
omitting it continues to suppress them. A `SendResponse` with an embedded
custom body remains unsupported, as do `InstanceId` and proposed meeting
start/end times. The behavior follows the
[MS-ASCMD MeetingResponse rules](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-ascmd/42ececcd-37df-4340-aa50-783ba714facb)
and [SendResponse definition](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-ascmd/128fff4c-1b97-42d8-a30f-f023327add0b).
Regression coverage asserts `sendSchedulingMessages` at the JMAP boundary and
updates the deferred live MeetingResponse scenario. Full suite: 267 passed,
73 ignored; clippy with warnings denied, formatting and diff checks pass.
Deployed as `deploy-2026-11-22-eas-16-meeting-send-response-1109860`; Hermes
Compose built image
`sha256:3e60bf151cf5b386a50cb564f9f899e8f327f8bf3b84070c2dee11908045e9d8`.
The container started at `2026-11-23T03:25:57Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **ActiveSync contact account name**. Contacts2 `AccountName`
round-trips as a separately labeled JSContact `onlineServices` user entry
(`eas_account_name`), preserving its distinct meaning from MMS and IM
addresses. Updates are state guarded and retain unrelated service entries.
Regression coverage checks WBXML parsing/output, change hashing, JMAP
create/read/update and preservation of unrelated services. Full suite: 266
passed, 73 ignored; clippy with warnings denied, formatting and diff checks
pass. Deployed as `deploy-2026-11-22-eas-contact-account-name-c9dc4a2`; Hermes
Compose built image
`sha256:5793ea78d050b5a84a36107f74f85264991beea946896b8d89a8c2fa2f453adf`.
The container started at `2026-11-23T03:18:46Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **ActiveSync contact MMS address**. Contacts2 `MMS` maps to
a dedicated JSContact `onlineServices` user value (`eas_mms`) without
conflating it with the three IM address fields. The field is defined as a
string address by [MS-ASCNTC](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-ascntc/a006c688-6d5f-4aed-a9ac-d70019d3c67b)
and JSContact permits a free-text `user` for online service data under
[RFC 9553](https://www.rfc-editor.org/rfc/rfc9553.html#section-2.3.2).
Regression coverage checks WBXML parsing/output, hash changes, JMAP
create/read/update, and preservation of other online services. Full suite:
266 passed, 73 ignored; clippy with warnings denied, formatting and diff
checks pass. Deployed as `deploy-2026-11-22-eas-contact-mms-b836553`; Hermes
Compose built image
`sha256:9bb3c332de0eb7605fce96ea2b894475c7c2982cdedcfe0d5cf8c57388573695`.
The container started at `2026-11-23T03:11:55Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **ActiveSync company main phone**. Contacts2 `CompanyMainPhone`
uses the standardized JSContact `phones[].features["main-number"]` value with
a stable `eas_company_main` ID; reads still accept the earlier label-based
representation. Partial updates replace or clear only this entry, preserving
other numbers. Regression coverage checks parsing, output, hash changes, JMAP
create/read/update and guarded merging. Full suite: 267 passed, 73 ignored;
clippy with warnings denied, formatting and diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-main-number-d5244e3`; Hermes Compose built image
`sha256:2c5f0587c046811b8ee42114159d181e46d1e6850deb032f29fe5a7c6dd99f53`.
The container started at `2026-11-23T03:39:25Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **ActiveSync contact IM addresses**. Contacts2 `IMAddress`,
`IMAddress2` and `IMAddress3` round-trip through JSContact `onlineServices`
`user` values with stable gateway-owned IDs and explicit EAS labels. Updates
replace or clear only those three entries and preserve unrelated services;
these fields are free-text identifiers, so no service protocol is inferred.
The shape follows [RFC 9553 section 2.3.2](https://www.rfc-editor.org/rfc/rfc9553.html#section-2.3.2).
Regression coverage checks WBXML parsing/output, change hashing, JMAP
create/read/update, clear semantics and retention of an unrelated Matrix
service. Full suite: 266 passed, 73 ignored; clippy with warnings denied,
formatting and diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-im-addresses-139399a`; Hermes Compose built
image `sha256:ebb267c7241ec5a8c8182d23134570d8e591d21b077a345a65e70d8c78d5dda0`.
The container started at `2026-11-23T03:00:18Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **ActiveSync contact nickname**. Contacts2 `NickName` maps to
JSContact `nicknames`, using a stable `eas_nickname` entry. Reads prefer that
entry and fall back to another nickname; partial updates replace or clear only
the EAS-owned entry and retain unrelated nicknames under the JMAP state guard.
Regression coverage checks WBXML parsing/output, change hashing, JMAP create,
read and a partial update that preserves another nickname. Full suite: 266
passed, 73 ignored; clippy with warnings denied, formatting and diff checks
pass. Deployed as `deploy-2026-11-22-eas-contact-nickname-c2dfd63`; Hermes
Compose built image
`sha256:e4e9135f3c360d49213a4c736a57c349f587531f4d04d90a4f772e2efb4b9b38`.
The container started at `2026-11-23T02:53:52Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **ActiveSync contact office location**. `OfficeLocation` maps
to a separate JSContact Address with its `full` value and an `office location`
label, keeping it distinct from the postal business address. Partial updates
replace or clear only that labeled address and retain other addresses under
`ifInState`. This uses the full-address representation defined by
[RFC 9553](https://www.rfc-editor.org/rfc/rfc9553.html) for the EAS string
field described in [MS-ASCNTC](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-ascntc/b58a3aa3-8131-4c7c-bb73-595271b8c2eb).
Regression coverage checks WBXML parsing/output, change hashing, and JMAP
create/read/update. Full suite: 266 passed, 73 ignored; clippy with warnings
denied, formatting, and diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-office-location-64d7a8e`; Hermes Compose built
image `sha256:f19c7893d805d7a0adc25a2ae8e0dc68fa3107d57d98271d328dc4c5d142c436`.
The container started at `2026-11-23T02:45:44Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`.

Also completed: **ActiveSync contact web pages**. EAS `WebPage` maps to a
JSContact `links` resource URI. Reads select HTTP(S) links, while updates
replace the current website entry and preserve unrelated links under
`ifInState`; empty values clear the website entry. The `links` property and
resource URI shape follow [RFC 9553](https://www.rfc-editor.org/rfc/rfc9553.html).
Regression coverage checks WBXML parsing/output and JMAP create/read/update.
Full suite: 266 passed, 73 ignored; clippy with warnings denied, formatting,
and diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-web-page-d97a2d2`; runtime verification is in
image `sha256:7ca1e14cadb0b1e8cfbd424afa9d0ad79847c15a5a0430393e516e43b8e35d20`,
started at `2026-11-23T02:08:18Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. No live contact was modified
during deployment.

Also completed: **ActiveSync business fax numbers**. EAS `BusinessFaxNumber`
maps to a separate JSContact phone entry with work context and the `fax`
feature. Partial updates merge it alongside other work and private phone
entries under `ifInState`. Regression coverage checks WBXML parsing/output and
JMAP create/read/update. Full suite: 266 passed, 73 ignored; clippy with
warnings denied, formatting, and diff checks pass. Deployed as
`deploy-2026-11-22-eas-contact-business-fax-78425f9`; runtime verification is
image `sha256:f63012bd75de6deeaa15f5d32243124326933a0ea4434089cef9a2d773150161`,
started at `2026-11-23T02:13:27Z` with restart count zero. Mesh `/healthz`
returned `ok` and `/readyz` returned `ready`. No live contact was modified
during deployment.

Also completed: **ActiveSync Contact postal addresses**. Business, home, and
other city/country/postal-code/state/street fields now round-trip through
JSContact `addresses` and preserve unrelated components during partial edits.
Empty tags clear the selected address component; server changes include empty
fields so deleted values clear on devices. Address edits use `ifInState` after
fetching and merging the current JSContact components. Regression coverage
checks WBXML parsing/output, JSContact conversion, create, partial update,
component preservation, clearing, and the state guard. Full suite: 264 passed,
73 ignored; clippy with warnings denied, formatting, and diff checks pass.
Deployed as `deploy-2026-11-22-eas-contact-addresses-0e8a071`; homelab GitOps
commit `74f155b` (rebased over two concurrent homelab commits). Compose built
image `sha256:67753a2287e9116631b99a8b31dbfcde26d5b3005f8964b6b29520ac2b6d0912`;
the container started at `2026-11-23T01:18:08Z` with restart count zero. Mesh
`/healthz` returned `ok` and `/readyz` returned `ready`. No live contact was
created during deployment.

This reconciliation supersedes older status claims in this document where
they conflict with source `main` at the stated revision. Update this section
after each completed feature and retain historical observations below as
evidence, not as current status.

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
| Sync: Mail (client Add) | **Done (`16.0`/`16.1`)** | `Email/set` / `Email/import` + blob | The mail Sync path creates drafts in Drafts and non-draft items in the requested Mailbox with stable replay markers, attachment handling, MIME import, response mapping, and recovery after a server write. Regression coverage includes draft replay/content changes/attachments/Bcc/retries plus non-draft property generation (`src/activesync/draft.rs`, `src/push_tests.rs`). |
| Sync: Mail (server-side delete surfaced to client) | **Done** (`deploy-2026-08-25v`) | Targeted `Email/get` (id + mailboxIds) per previously-seen-but-absent-from-window candidate | `emails_still_in_mailbox()` in `src/jmap/client.rs` — deliberately NOT a naive "absent from this fetch = deleted" diff, since `emails_in_mailbox`'s query is sorted newest-first and capped at the window size, so an old message pushed out of the window by newer mail looks identical to a real deletion under a plain diff. Live-verified both directions: a real destroyed message correctly surfaced as a Delete on the next Sync, and a real non-deleted message correctly confirmed present (not flagged) via the same JMAP call shape. |
| Sync: Contacts (read) | **Done** | `ContactCard/query`+`/get` (JSContact, RFC 9553) | Confirmed live against a real card. |
| Sync: Contacts (write: add/edit/delete) | **Done** (`deploy-2026-08-25s`, addresses `deploy-2026-11-22-eas-contact-addresses-0e8a071`, categories `deploy-2026-11-22-eas-contact-categories-79585fc`, names `deploy-2026-11-22-eas-contact-name-parts-c3f4378` and `deploy-2026-11-22-eas-contact-name-prefix-c48ee61c`, dates `deploy-2026-11-22-eas-contact-dates-5f03d66`, department `deploy-2026-11-22-eas-contact-department-6708d25`, secondary phones `deploy-2026-11-22-eas-contact-secondary-phones-6c3d00d`, fax/pager `deploy-2026-11-22-eas-contact-fax-pager-9e7ab32`, car/radio `deploy-2026-11-22-eas-contact-car-radio-60e5eca`, assistant phone `deploy-2026-11-22-eas-contact-assistant-phone-0a89e0a`, phonetic names `deploy-2026-11-22-eas-contact-yomi-name-1a55270` and `deploy-2026-11-22-eas-contact-yomi-company-4860c47`, office location `deploy-2026-11-22-eas-contact-office-location-64d7a8e`, webpage `deploy-2026-11-22-eas-contact-web-page-d97a2d2`, business fax `deploy-2026-11-22-eas-contact-business-fax-78425f9`) | `ContactCard/set` | `sync_contacts_collection` handles Add/Change/Delete and hash diff. Contact names and phonetic names, birthdays/anniversaries, emails, home/business primary and secondary lines, fax/pager/car/radio/assistant numbers, web pages, organization/title/department, office location, business/home/other postal address components, and categories map to JSContact. Partial address, name, organization, phone, link, and anniversary edits preserve untouched components under a JMAP state guard; empty fields clear components. |
| Sync: Calendar (read) | **Done** | `CalendarEvent/query`+`/get` (JSCalendar, draft-ietf-calext-jscalendarbis superseding RFC 8984) | Confirmed live against a real event; recurrence is now mapped where supported. |
| Sync: Calendar (write: add/edit/delete) | **Done** (`deploy-2026-08-25t`; recurrence implementation pending deployment) | `CalendarEvent/set` | `save_calendar_event`/`destroy_calendar_event` handle lifecycle and supported recurrence rules. `start`/`duration` retain existing timezone conventions. Attendees, reminders, and per-instance recurrence exceptions remain out of scope. |
| Sync: Calendar recurrence (RRULE) | **Done (`deploy-2026-11-24-eas-calendar-recurrence-bea1387`)** | Stalwart singular JSCalendar `recurrenceRule` | Common EAS daily, weekly, monthly, nth-weekday, and yearly rules map both ways. Fresh localhost-only Stalwart 0.16.27 and 1.2.0 probes verified singular-property create/get. The August probe tested plural `recurrenceRules`, which was the wrong Stalwart property. Per-instance exceptions remain unsupported. |
| Sync: Tasks | **Done, two-way (`deploy-2026-08-26i`); recurrence mapping (`deploy-2026-11-24-eas-task-recurrence-a28032b`) | `CalendarEvent` with `@type: "Task"` — confirmed live, not a separate JMAP object | Existing Add/Change/Delete lifecycle is live-verified over WBXML. Common EAS recurrence patterns map through the shared JSCalendar `recurrenceRule` conversion; local regressions cover WBXML parse/write and JMAP Task-object conversion. Task-specific Stalwart create/get and real-client recurrence behavior remain unverified. Per-instance recurrence overrides are rejected. The Task field order still needs real-device verification. |
| Sync: Notes | **Done, two-way** | Email-backed synthetic-message workaround (`src/jmap/notes.rs`) | The most structurally interesting piece of this whole gateway — see that file's module doc. Full Add/Change/Delete supported. |
| FolderSync | **Done, incremental** | `Mailbox/get`, `AddressBook/get`, `Calendar/get` | Key 0 sends a full Adds snapshot; subsequent keys diff per-device collection fingerprints into Adds/Updates/Deletes. Unknown keys return Status 9. |
| FolderCreate/Update/Delete | **Done** (`deploy-2026-11-22-eas-groupware-folders-094b478`) | `Mailbox/set`, `AddressBook/set`, and `Calendar/set` | Mail folders retain hierarchical parent support. User Contacts (`Type=14`) and Calendar (`Type=13`) folders create, rename, and delete JMAP collections with synthetic `ab_`/`cal_` EAS IDs; those collections remain root-level because JMAP has no parent hierarchy. Notes and Tasks remain derived collections without independent CRUD. Mocked HTTP regression covers create/rename/delete for both types and rejects invalid types. |
| Ping | **Done** | N/A (gateway-local long-poll + JMAP poll underneath, implementation detail not deeply re-read this pass) | Empirically rock-solid over the entire multi-day A/B test; deliberately not touched further this session even where a codepage table read once looked mismatched. |
| ItemOperations (Fetch) | **Done (legacy/secondary path)** | `Email/get` + `download_blob()` | Real device confirmed to use the *Sync-embedded* Fetch instead for opening messages; this command path exists for other-client compatibility. |
| ItemOperations (attachment fetch via ItemOperations, not GetAttachment) | **Implemented** | `download_blob_with_type()` | `FileReference` fetch supports full and byte-range blob responses. ItemOperations Fetch also routes notes, tasks, calendar events, contacts, and mail. |
| GetAttachment (legacy) | **Done** | `download_blob()` | `AttachmentName` reference is a synthetic `blobId||name` string this gateway itself issues. |
| GetItemEstimate | **Done** | Counts from the same `Email/query`/`ContactCard/query`/`CalendarEvent/query` paths used for Sync | FolderType field bug fixed this session. |
| MoveItems | **Done (mail only), non-mail behavior confirmed safe** | `Email/set` (`mailboxIds`) | Mail only. Confirmed by direct code read (no longer unverified): a Notes/Contacts/Calendar item would fail cleanly with Status 5 (`Email/set` rejects the non-Email id as `notFound`), not silent corruption. Currently moot anyway — every non-mail collection has exactly one folder, so no real client has anywhere to offer moving an item to. See command-matrix doc for the full reasoning. |
| Search (mailbox + GAL) | **Done, both halves (`deploy-2026-08-26n`)** | `Email/query` `filter:{text:...}` (mailbox); `Principal/query` `filter:{text:...}` (GAL) | Both live-tested BEFORE writing any handler code, not assumed: `Email/query`'s `text` filter does real full-text matching (a probe search matched a message only in its body, not the subject); `Principal/query`'s `text` filter matches against the 7 real principals on this instance (not an empty directory) by whole-token match (a short prefix of "khuo.ng" matched nothing; the full first name "karen"/"khuong" matched exactly that principal) -- weaker than real Exchange GAL's substring/prefix ANR, but genuinely working. `search()` (`activesync.rs`) dispatches `Store>Name` to the right one; a Mailbox search restricted to any `airsync:Class` other than "Email" returns a clean empty result rather than erroring. `EAS Range`'s zero-based-inclusive "m-n" converts directly to JMAP `position`/`limit`. Real bug caught during live verification, not left in: the first pass reused `write_email_fields`'s `body_pref=None` (its own doc comment: "always full body -- correct for ItemOperations Fetch"), so a 4-result search came back as a 132KB response with every match's full HTML body and attachment metadata inlined -- fixed by parsing Search's own `airsyncbase:BodyPreference` (the identical element Sync's Options carries) and defaulting to a 500-char plain-text preview when absent, confirmed live to shrink the same 4-result response to 2.9KB. `ItemOperations`'s Fetch parser was also extended to accept a bare `search:LongId` (confirmed against the real Fetch (ItemOperations) schema as a genuine alternative to CollectionId+ServerId, not an addition) so a device can open a search result's full body afterward -- live-verified: fetched the top mailbox-search result by its LongId and got the full email back via the same `get_email_by_id` path Sync-triggered Fetches use. `Find` (the OTHER command that could theoretically back a search UI) stays unimplemented and was removed from `SUPPORTED_COMMANDS` -- every WBXML tag in its codepage is spec-gated to protocol 16.1, which this gateway doesn't advertise, so it was dead advertising, not a real gap. |
| SendMail | **Done** | `EmailSubmission/set` (blob-upload then submit) | Confirmed live and working. |
| SmartReply / SmartForward | **Done, threading fidelity confirmed** | Same `EmailSubmission/set` path as SendMail | Live-tested this session with a self-addressed message carrying a real Message-ID/In-Reply-To/References triplet: byte-identical values confirmed on both the Sent Items copy and the delivered Inbox copy. See roadmap item 4 for the full test. |
| ResolveRecipients / ValidateCert | **Implemented, end-to-end covered** | Principal/contact directory matching; local X.509 chain validation | `src/activesync/resolve_recipients.rs` and `src/activesync/validate_cert.rs`; regression coverage in `src/push_tests.rs`. S/MIME compose/signing remains separate and is not implied by these commands. |
| Settings: DeviceInformation / UserInformation | **Done (Get)** | Static/echo — no real JMAP-backed device-info storage | `PrimarySmtpAddress` bug (unrecognized field, cascading parse failure) fixed this session by removing it; `EmailAddresses>SMTPAddress` carries the same info in a valid shape. |
| Settings: Oof (Get) | **Done** *(disabled path live-verified; enabled path spec-derived, not device-verified)* | `VacationResponse/get` (`src/jmap/vacation.rs`) | Real account state now read on every Get. Disabled (`OofState=0`) shape confirmed live, unchanged from the earlier z-push comparison fix. Enabled shape's `OofMessage` block is built from MS-ASSETTINGS' own schema, not live-toggled -- see `src/jmap/vacation.rs` module doc for why (toggling `isEnabled=true` on the real account risks a genuine auto-reply going out unsupervised). Verify with a real device present before fully trusting the enabled path. |
| Settings: Oof (Set) | **Done, persists** | `VacationResponse/set` (`src/jmap/vacation.rs`) | Live-verified end to end with `isEnabled=false` (safe -- confirmed via direct JMAP query that `subject`/`textBody` actually persisted to the real `VacationResponse.singleton` object, then cleaned back up). `isEnabled=true` was deliberately never live-toggled for the same reason as the Get path above. |
| Settings: RightsManagementInformation (IRM) | **Missing, out of scope** | N/A | Enterprise feature, not relevant to this account. |
| Provision | **Done** | N/A (gateway-local, accept-everything policy) | PolicyStatus bug fixed this session. |
| MeetingResponse | **Implemented, with protocol-version-specific target handling** | JMAP invitation lookup and `CalendarEvent/set` participation updates | Inbox invitation path and calendar target handling are in `src/activesync/meeting_response.rs`; end-to-end tests cover acceptance, tentative, decline, and invalid targets. The 14.0 restriction on modifying calendar-folder requests is enforced. |
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

2b. ✅ **DONE (`deploy-2026-11-22-eas-groupware-folders-094b478`) — FolderCreate/FolderUpdate/FolderDelete for mail, contacts and calendars.** Mail remains hierarchical; groupware collections use `AddressBook/set` and `Calendar/set` at the root. Regression tests cover both collection types. See the status table row for limitations and deployment details.
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
5. **SUPERSEDED FINDING (2026-11-24) — Calendar recurrence.** The August
   probe correctly observed that plural `recurrenceRules` was rejected, but
   tested the wrong property for Stalwart. Fresh localhost-only probes found
   that Stalwart 0.16.27 and 1.2.0 accept and return singular `recurrenceRule`.
   The gateway now implements common EAS recurrence mappings; exceptions and
   patterns that cannot be represented safely remain unsupported. See the
   current reconciliation above and `src/jmap/calendar_recurrence.rs`.
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

7. ✅ **DONE (`deploy-2026-08-26n`) — turned out NOT to need a workaround
   at all, wrong section.** `Principal/query`'s `text` filter genuinely
   works, live-confirmed against the 7 real principals on this instance —
   see the Search row in the status table above for the full build and
   verification story (both mailbox and GAL halves, plus a real bug caught
   and fixed live: search results were sending full bodies until Search's
   own `BodyPreference` got parsed).

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
