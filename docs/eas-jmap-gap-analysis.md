# EAS ↔ JMAP gap analysis and roadmap

## Reconciled status for source `main` (2026-11-24)

The historical tables and roadmap below describe an August 2026 source snapshot;
they are not a current implementation inventory. The November source contains
draft create/edit with stable EAS identity, versioned flags and their metadata,
Find (mailbox and GAL), versioned calendar fields, multiple body preferences,
MIME policy, partial Sync/options persistence, and Sync long-poll/window
handling. Check the current Rust implementation before treating any older
“Missing” or “Not implemented” statement below as actionable.

Current prioritized work:

Find now extracts Excel Binary Workbook (`.xlsb`) cell values through the
existing vendored Calamine parser, including its registered macro-enabled
content type. A real Calamine XLSB fixture verifies extraction of worksheet
cells; malformed or unsupported workbooks fail closed. Regression and full
suite: 405 passed, 75 ignored; strict Clippy, formatting, and diff checks
pass. Deployed as `deploy-2026-11-24-eas-find-xlsb-c11b2cd`; Homelab commit
`31ed9de`, image `sha256:cac5c117d2da26f63c8564d83bf78e613b03eaf7ab372cf3e17d1497fc2e2bb2`,
started `2026-11-25T03:17:44Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`. Production remains at the 14.0 protocol default.

Find raster attachment OCR is implemented for PNG, JPEG, GIF, TIFF, BMP, and
WebP. Image dimensions are checked before decoding (12,000 pixels per edge,
40 million pixels total), decoding allocations are capped, images are reduced
to a 1,600-pixel edge, and Tesseract has a five-second budget. A regression
uses a generated PNG and a controlled OCR executable, plus malformed and
over-dimension inputs. A real Tesseract OCR fixture passes when explicitly
run; it is ignored in the default suite because Tesseract is an optional test
runtime. The full suite passes 406 tests (76 ignored), strict Clippy,
formatting, and diff checks. Deployed as
`deploy-2026-11-24-eas-find-image-ocr-1218602`; Homelab commit `ccb35a7`,
image `sha256:c5abe248397812ef210d416435b1406dc29ba50f3202bd13fc0323dfd2cd6e2f`,
started `2026-11-25T03:29:02Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`. The container reports Tesseract `eng` and `osd` language
data. Production remains at the 14.0 protocol default.

Outlook `.msg` Find now falls back to bounded compressed-RTF decoding when
plain text and HTML bodies are absent. The compressed and declared output sizes
are checked against the existing 16 MiB limits before decompression. An
upstream Outlook fixture verifies RTF text extraction and rejection of an
oversized declared output. The focused regression and strict Clippy pass; the
full suite passes 406 tests (76 ignored). Deployed as
`deploy-2026-11-24-eas-find-outlook-rtf-1b666af`; Homelab commit `93cce16`,
image `sha256:da8f53bdcb9abde4adf2926605e84d122ca3ec9d5e40ae2a9bec41da3b8c4dab`,
started `2026-11-25T03:45:09Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`. Production remains at the 14.0 protocol default.

Completed since reconciliation: **Task recurrence Add/Fetch**, including
the encoded Sync command parser path, is now live-verified against the
isolated Stalwart bridge. EAS Add creates a Task with the expected daily
`recurrenceRule`; an explicit Sync Fetch reads that recurrence back as Task
WBXML. The test exposed two issues: incomplete Tasks must omit the non-nullable
`progressUpdated` property, and Task Sync queries must request
`recurrenceRule`. Both fixes have focused regression coverage. The live E2E
test passes; the full suite passes 404 tests (75 ignored), strict Clippy and
formatting pass. Real-client iOS/Outlook behavior remains part of the separate
certification gate below. Code commit `bdd323e` is deployed as
`deploy-2026-11-25-eas-task-recurrence-bdd323e`; Homelab commit `4b59842`,
image `sha256:9c0b5083e9a20c41b39847f2eafb775889e17a3fda16350023693939f4483798`,
started `2026-11-25T01:44:23Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`. Production has no protocol-version override and retains the
14.0 default.

Find permits up to 16 MiB of extracted attachment text, downloads attachments
up to 16 MiB, and expands generic ZIP entries up to 16 MiB. Exact and over-limit
text boundaries are covered, and a 12 MiB plain-text attachment is found by an
exact NEAR query. The full suite passes 404 tests (75 ignored), strict Clippy
passes, and the real scanned-PDF OCR test passes. The text-cap change is
deployed as
`deploy-2026-11-25-eas-find-8mb-text-0f5c90c`; Homelab commit `589dbd9`, image
`sha256:18092476150a2781c8bc06424780f7eb50bdf66c32cdd99f387ea8893248cf6a`,
started `2026-11-25T01:53:44Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`. Production remains at the 14.0 protocol default.

The download ceiling is now 16 MiB while extracted text remains capped at
8 MiB. A 12 MiB OOXML package is searchable, while a metadata-declared
attachment above 16 MiB is verified to be skipped before download. The full
suite passes 404 tests (75 ignored); strict Clippy, formatting, and diff checks
pass. Deployed as `deploy-2026-11-25-eas-find-16mb-input-a569207`; Homelab
commit `65735eb`, image
`sha256:d4f8502b4a4e4b9c3cc04f57fb08cc89a953ed8fefbef9c49763d4f8a5ee6611`,
started `2026-11-25T02:00:32Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`. Production retains the 14.0 protocol default.

Generic ZIP extraction now permits up to 8 MiB of expanded allowlisted content,
matching the extracted-text ceiling. Exact and over-limit archive boundaries
are tested, and a 3 MiB expanded ZIP is searchable through Find. The full suite
passes 404 tests (75 ignored), strict Clippy passes, and formatting and diff
checks pass. Deployed as `deploy-2026-11-25-eas-find-zip-8mb-702fcd9`;
Homelab commit `f89d6f0`, image
`sha256:68b98de476f8fd0434c2b6085968cef904bd90209bbe6bf56a0dbc5a60617c09`,
started `2026-11-25T02:10:46Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`. Production retains the 14.0 protocol default.

PDF Find OCR now permits one hundred image-only pages per attachment with a
30-second total runtime budget. The synthetic tool regression proves one
hundred pages are extracted, 101 are refused, and each render/OCR subprocess
is bounded by both its per-command timeout and remaining total budget; existing
real scanned and mixed-PDF fixtures cover the installed Poppler/Tesseract path. The full
suite passes 404 tests (75 ignored), strict Clippy passes, and formatting and
diff checks pass. Deployed as
`deploy-2026-11-25-eas-find-ocr-100pages-16eff40`; Homelab commit `b36691a`,
image `sha256:977e1b49ad33b669a509cf55f1b9a20c1caffef5201c1cb0fe604f53ed39b840`,
started `2026-11-25T03:04:16Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`. Production retains the 14.0 protocol default.

Generic ZIP Find now also extracts common source-code files such as `.rs`,
`.py`, `.js`, `.ts`, `.go`, and `.java`, under the same archive expansion and
text-output bounds. The exact-search extraction regression verifies source
files are included while binary files remain skipped. An EAS Find regression
also finds a unique phrase inside a Rust source file in a ZIP attachment. The
full suite passes 404 tests (75 ignored), strict Clippy passes, and formatting
and diff checks pass.
Deployed as `deploy-2026-11-25-eas-find-zip-source-adf5a0d`; Homelab commit
`4cc4b37`, image
`sha256:2b0061d9d30ec8d7a9f6dcb8b09248972d388ba26b63a288c7ce9895c4982992`,
started `2026-11-25T02:25:59Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`. Production retains the 14.0 protocol default.

Find's extracted-text and generic ZIP expansion limits now both match the
16 MiB attachment download ceiling. Exact 16 MiB and over-limit boundaries
are tested; EAS Find matches exact NEAR terms in 12 MiB plain-text and expanded
ZIP attachments. The full suite, strict Clippy, formatting, and diff checks
pass. Deployed as `deploy-2026-11-25-eas-find-16mb-extract-1730245`;
Homelab commit `4c22112`, image
`sha256:0d757e377c26519c0c080d231304335fcace4e700981cc527536f30d4c4cdb33`,
started `2026-11-25T02:42:18Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`. Production retains the 14.0 protocol default.

Remaining 16.1 readiness backlog (ordered by runtime impact):

1. **Real-client 16.1 validation.** Exercise negotiation, Tasks, BodyPart,
   Find, and enabled Oof on the isolated test bridge. The gateway now supports
   an explicit `EAS_MAX_PROTOCOL_VERSION=16.1` test configuration and includes
   Find in the 16.1 command advertisement. The default remains 14.0; production
   continues to advertise 14.0 until the client pass succeeds. A local WBXML
   regression now checks the enabled Oof Get response's scheduled state,
   end-time shape, and three audience-specific message blocks; it does not
   replace checking the enabled state with a real client. The full current
   suite passes 404 tests (75 ignored); strict Clippy, formatting, and diff
   checks pass.
   Deployed with that production default as
   `deploy-2026-11-24-eas-16.1-opt-in-13bea13`; homelab commit `fcb698e`, image
   `sha256:902bfe01a2ec516b68b7517f9a38ba730bfafdeb66eb5cd5ec85822ef4e44b0d`,
   started `2026-11-24T23:30:09Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The container has no protocol override, so the live
   production endpoint continues to use the tested 14.0 default.
2. **Find content coverage.** Current bounded extraction supports legacy `.xls`,
   binary Excel `.xlsb`,
   `.ppt`, and `.doc`, EPUB, macro-enabled OOXML, RFC 822 messages, Outlook `.msg`
   attachments including compressed-RTF fallback, ordinary ZIP archives with allowlisted text/config/source
   files, and
   Word, Excel, and PowerPoint package comment text, plus
   MIME-declared charsets for text attachments and XML declaration encodings.
   Scanned-only PDFs with up to one hundred image-only pages and supported
   raster image attachments use resource-bounded OCR. Attachments above 16 MiB and PDFs with more than one hundred image-only
   remain outside exact local matching. Mixed
   PDFs retain native text and OCR only pages without extractable text, even
   when the PDF has more than three total pages. A four-page real fixture
   covers native text plus an OCR-only page.
   The attachment input cap was raised from 4 MiB to 8 MiB
   with a regression that searches inside a 6 MiB OOXML payload; the focused
   test and full suite passed. Deployed as
   `deploy-2026-11-24-eas-find-8mb-bedb85e`; homelab commit `a4a3ab9`, image
   `sha256:9df94985f6eb239b3f8bd17bdf397a9c8df465c97744ea09934ac423d48f2206`,
   started `2026-11-25T00:29:19Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Protocol advertisement remains at the 14.0 default.
   Mixed-PDF page extraction is also deployed as
   `deploy-2026-11-24-eas-find-mixed-pdf-a0afdab`; homelab commit `1412218`,
   image `sha256:584e54604e4d29893254533e60fe59ccc9fd00f3507aa9acb7c73bf7c813dfb4`,
   started `2026-11-25T00:39:31Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The expanded suite passes 402 tests (75 ignored), strict
   Clippy and formatting pass. Real OCR passes against both the scanned-only
   fixture and a four-page mixed native/scanned fixture. The OCR budget now
   counts image-only pages, not total PDF pages, and renders only those pages.
   Deployed as `deploy-2026-11-24-eas-find-ocr-pages-0dadd16`; homelab commit
   `d1f6d6a`, image
   `sha256:9699a05e302248912a5fc228be3b9ab53aeac297ed8e55d46c254f03ced6e763`,
   started `2026-11-25T00:53:12Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Production continues to advertise 14.0 by default.
   The exact extracted-text cap was raised to 4 MiB; generic ZIP expansion remains
   independently capped at 2 MiB. Regressions cover exact/over-limit text and
   the unchanged ZIP expansion bound. The full suite passes 403 tests (75
   ignored), strict Clippy and formatting pass, and real OCR passes on the
   four-page mixed fixture. Deployed as
   `deploy-2026-11-24-eas-find-4mb-text-2195aae`; homelab commit `e1c08ab`,
   image `sha256:12c48fd5d38edf55c83b8cfb3fdcf8a388c78fa84c2b67cf1127d2a0993d0e79`,
   started `2026-11-25T01:02:10Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Production remains at the 14.0 protocol default.

1. **BodyPartPreference / BodyPart response path — implemented and deployed.**
   Request parsing and independent Sync option
   persistence are implemented. Sync and Search now prepare and emit a
   separate `AirSyncBase:BodyPart` using the requested type, truncation,
   AllOrNone, and preview settings. ItemOperations fetches do the same for
   protocol 14.1 and 16.0; its Properties schema explicitly excludes BodyPart
   for 16.1, so that operation omits it there even though Sync and Search
   support it in 16.1. The response writer uses the required Status, Type,
   EstimatedDataSize, Truncated, Data, Preview order. Wire-level regressions
   exercise Search and Sync at 16.1 and ItemOperations across 14.0, 14.1, 16.0,
   and 16.1. The saved `Email.bodyStructure` `partId` mapping (`cd0119b`) is
   retained for attachment records; it is not a selector in the
   `BodyPartPreference` schema and is not required to answer these requests.
   The current deployment tag
   `deploy-2026-11-24-eas-oof-partial-b067da4` includes this path; deployed
   with homelab commit `b8be778`, image
   `sha256:53b9c323df006c8191a44362f91343882af38fb15f7508a4a59c51456cbdd589`,
   started `2026-11-24T21:23:21Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Real-client 16.1 behavior remains unverified. Sources:
   Microsoft [MS-ASCON Elements](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-ascon/605bd946-0750-4d9b-b04c-8be28badfa8e),
   [MS-ASAIRS Appendix A](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-asairs/fbbc1af9-5a71-4925-aca8-60a95339c722),
   [MS-ASCMD ItemOperations Properties](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-ascmd/b6b1551a-f841-4bac-967c-a3328af78229),
   and [MS-ASCMD Common Status Codes](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-ascmd/95cb9d7c-d33d-4b94-9366-d59911c7a060).

2. **Find/KQL parity.** Standalone `subject:"multiword phrase"` now uses
   Stalwart's subject index for candidates and locally enforces case-insensitive
   adjacent word order, including accurate totals and range paging. Regression
   coverage proves that reversed words and intervening words are excluded.
   Mailbox Find responses include the EAS `Importance` property mapped from
   the JMAP message importance header (0 low, 1 normal, 2 high), with a
   regression checking all three values. Regression suite: 382 passed, 73
   ignored; strict Clippy, formatting, and diff checks passed. Deployed as
   `deploy-2026-11-24-eas-find-importance-c79fa3b`; homelab commit `4c2eb86`,
   image
   `sha256:150ae046978987f3222b14b3ca3115f0a5cbf8f2ce9a9694e9dbf0bc830d2e17`,
   started `2026-11-24T19:33:29Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
   GAL Find uses the EAS prefix-string/ANR rule locally across principal
   display names, aliases, and email tokens; multi-token prefixes such as
   `Michael A.` now match `Michael Alexander` despite JMAP's whole-token
   Principal text filter.
   Explicitly ANDed phrases on the same property (for example two distinct
   `subject:"..."` terms) are also exact-filtered together; adjacent repeated
   same-property phrase restrictions without `AND` are exact-filtered as OR
   groups, matching MS-KQL grouping. Deployed as
   `deploy-2026-11-24-eas-kql-same-property-phrase-or-d725eae`; homelab commit
   `2e8cf77`, image
   `sha256:b95951125b4ba0a0209ff365cca0c006ec57ac3f696c10d54653f1b2776903e0`,
   started `2026-11-24T13:24:32Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Implicitly grouped same-property phrases can be combined
   with supported boolean restrictions and are locally evaluated with KQL's
   OR-within-property, AND-between-property semantics. Deployed as
   `deploy-2026-11-24-eas-kql-implicit-phrase-groups-15f21fb`; homelab commit
   `fc7db64`, image
   `sha256:3eec3a90858d790a89b559e2ab2aded5932dfd31b476b27c510b29bf63ee87a5`,
   started `2026-11-24T13:31:15Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. KQL `WORDS(...)` can be ANDed with an exact phrase and
   retain its JMAP candidate restriction. Positive phrase expressions retain
   safe JMAP candidates while exact local phrase checks preserve result
   semantics. Deployed as
   `deploy-2026-11-24-eas-kql-safe-phrase-candidates-2cc3025`; homelab commit
   `4c6baf8`, image
   `sha256:004107e934579c29c2627e8ebf1fb726be5200f24f572d2c272319386f0a4c39`,
   started `2026-11-24T13:37:12Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. Phrase OR expressions now also accept
   explicit `NOT` and unary-minus phrase alternatives; these broad-scan the
   scoped mailbox and evaluate each phrase branch exactly. Deployed as
   `deploy-2026-11-24-eas-kql-negated-phrase-or-fdbf559`; homelab commit
   `HOMELAB_COMMIT`, image
   `sha256:f6bec07796a29c5b82d5da2bcaa024eb1d492fa3153f464360d49a3d63867fd5`,
   started `2026-11-24T13:52:06Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. Parenthesized same-property phrase OR
   groups can now participate in mixed expressions, including conjunctions
   with supported boolean restrictions. Ambiguous repeated-property forms
   remain rejected. Deployed as
   `deploy-2026-11-24-eas-kql-repeated-phrase-or-group-f159afe`; homelab commit
   `afef4bf`, image
   `sha256:c7b3e83b9b03a5a1f2eaee504bc2c31b9cdddbae177f2e7cc97646110dd854e0`,
   started `2026-11-24T13:59:06Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. Quoted phrases now also support `from`,
   `to`, `cc`, `bcc`, and `participants`, with exact local matching against
   the returned address fields. Deployed as
   `deploy-2026-11-24-eas-kql-address-phrases-c112271`; homelab commit
   `0ef4ef0`, image
   `sha256:36be86745690798648bb95a84e477382be222402b86561b484dc637295970604`,
   started `2026-11-24T14:06:28Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. Phrase expressions also support the
   standard `importance:low|normal|high` predicate, including OR alternatives
   and negated-phrase expressions; JMAP candidate filtering is retained when
   safe, and importance is checked locally in broad scans. Deployed as
   `deploy-2026-11-24-eas-kql-phrase-importance-a2d0afe`; homelab commit
   `3f49a30`, image
   `sha256:57d4f01f19c17679640abe7fb23b78349c8e7755b72889e1a7c9ec4fb2005fd1`,
   started `2026-11-24T14:15:22Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. Same-property phrase alternatives may
   now share a parenthesized OR group with supported boolean alternatives,
   then combine with an outer boolean restriction. Deployed as
   `deploy-2026-11-24-eas-kql-phrase-group-boolean-12ded9c`; homelab commit
   `b49a401`, image
   `sha256:65ca8b2e80e004139f1754dbd580252a373a4f627b7a48b2563fd5fc2fdc0608`,
   started `2026-11-24T14:22:38Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. Boolean equality spellings such as
   `isread=true` and `hasattachment=false` are accepted in the same phrase
   expressions as colon-form predicates. Deployed as
   `deploy-2026-11-24-eas-kql-phrase-boolean-equality-d17467e`; homelab commit
   `e0576e5`, image
   `sha256:0a307e52388f5116b69625dc33c423879efca96777dc413e8bdc45089e74dab9`,
   started `2026-11-24T14:28:01Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. Boolean inequality syntax such as
   `isread<>false` is also evaluated exactly alongside phrase branches.
   Deployed as
   `deploy-2026-11-24-eas-kql-phrase-boolean-inequality-1cce6dc`; homelab
   commit `6e9f9fd`, image
   `sha256:a30a2cbdbbc32a96a0db093b11afcc3ec7d6fe107a0e0b3aa4d13fb80dfaf947`,
   started `2026-11-24T14:33:12Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. `received` date restrictions may also
   appear as phrase-expression alternatives; exact local evaluation uses the
   same normalized UTC JMAP filter as candidate selection. Deployed as
   `deploy-2026-11-24-eas-kql-phrase-received-date-e241abb`; homelab commit
   `c43adf0`, image
   `sha256:851a5a222f30126cb785210650169954930290164378fcdf89876f255cf43b6e`,
   started `2026-11-24T14:41:36Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. KQL size predicates such as `size>100`
   and `size:10..20` can now be combined with quoted phrases; candidate filters
   use JMAP `minSize`/`maxSize`, and local post-filtering preserves inclusive
   and exclusive boundaries, including negated size conditions. Regression
   suite: 335 passed, 73 ignored; strict Clippy, formatting, and diff checks
   passed. Deployed as
   `deploy-2026-11-24-eas-kql-phrase-size-negation-9b61ba2`; homelab commit
   `238b77e`, image
   `sha256:41d4d9c36a880e8a36533c89f77d387ba78502872ce033898e53557a4bc0e2b8`,
   started `2026-11-24T14:56:15Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. KQL `sent` date restrictions now filter from the JMAP
   Email `sentAt` property, including UTC-day boundaries and mixed phrase
   expressions; because JMAP has no equivalent `sentAt` query condition, Find
   scans candidates in the selected mailbox and applies the exact local date
   filter. Regression suite: 338 passed, 73 ignored; strict Clippy, formatting,
   and diff checks passed. Deployed as
   `deploy-2026-11-24-eas-kql-sent-date-81b1409`; homelab commit `b0367aa`,
   image `sha256:b8ae02312614bb4c6ec41932d72ced10a18759a767932c235bc7f3c7e25648ee`,
   started `2026-11-24T15:09:22Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Importance comparisons now accept `=`, `<>`, `<`, `<=`,
   `>`, and `>=` with local low/normal/high ordering; JMAP header equality
   filters narrow safe candidates while ordered and negated comparisons scan
   the selected mailbox and apply exact post-filtering. Regression coverage
   checks comparison boundaries, negation, OR with a boolean predicate, result
   IDs, and totals. The full suite passed 341 tests (73 ignored); strict Clippy,
   formatting, and diff checks passed. Deployed as
   `deploy-2026-11-24-eas-kql-importance-comparisons-bd30d88`; homelab commit
   `118c119`, image
   `sha256:5e3d9e325a56e4aff841ab779f5207c2ac835f1f3535456a02150b630ec64ed2`,
   started `2026-11-24T16:03:06Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The standalone negated form
   `NOT subject:"..."` and unary-minus forms such as
   `-body:"..."` are exact-filtered over the scoped mailbox. Positive phrase
   AND negated boolean restrictions (`NOT isread:true` or `-isflagged:true`)
   retain JMAP's exact boolean filter while exact-checking the phrase. A positive phrase AND a negated phrase is exact-filtered by scanning the
   scoped mailbox; `isread`, `isflagged`, and `hasattachment(s)` predicates
   can now accompany it and are evaluated locally during that scan. Other
   unsupported predicates in that broad-scan form remain rejected. Deployed
   as `deploy-2026-11-24-eas-kql-negated-phrase-boolean-b5e0e06`; homelab
   commit `9ccede3`, image
   `sha256:c30663005e9035e4736ccff0af951182a5004753977cad5931e8c0b0ef6612e4`,
   started `2026-11-24T13:44:08Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. A positive phrase OR boolean filter is also
   supported for `isread`, `isflagged`, and `hasattachment(s)` values, with
   exact local evaluation of both branches. Multiple boolean alternatives
   may accompany positive phrase terms in a flat OR expression, including
   unary-minus and explicit `NOT` boolean alternatives. Flat explicit
   AND/OR expressions over phrases and supported boolean restrictions honor
   AND precedence through exact local evaluation, including explicitly
   parenthesized groups and implicit groups around explicit operators when
   supported properties occur once. Repeated properties in mixed explicit
   AND/OR expressions and unsupported qualifiers remain rejected. Regression
   coverage checks read,
   flagged, multiple boolean branches, and multiple phrase branches while
   excluding reversed phrase candidates. Deployed as
   `deploy-2026-11-24-eas-kql-negated-bool-or-81fdc6c`; homelab commit
   `b681fae`, image
   `sha256:9effefda5525d695bfb3e561cc4759d479e42df8910a340d2574adbd960c62cc`,
   started `2026-11-24T12:43:36Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Explicitly parenthesized groups of phrase and boolean
   terms in mixed AND/OR expressions are deployed as
   `deploy-2026-11-24-eas-kql-phrase-boolean-groups-1d43ee4`; homelab commit
   `e613031`, image
   `sha256:850bd2508f5fb030ea74150feb7889ee33f1bac7d6a34a164c7d4349af0d12fb`,
   started `2026-11-24T13:12:06Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Implicitly grouped phrase and boolean terms surrounding
   explicit AND/OR operators now preserve KQL precedence when properties are
   unique. Deployed as
   `deploy-2026-11-24-eas-kql-implicit-mixed-69bc376`; homelab commit
   `7d46a31`, image
   `sha256:cec7a7bb258846501024de6add1a20b6c366eb81ba7684719a707c9e598111c8`,
   started `2026-11-24T13:20:17Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. Flat mixed AND/OR phrase queries are deployed as
   `deploy-2026-11-24-eas-kql-phrase-boolean-precedence-d338991`; homelab
   commit `0a847c1`, image
   `sha256:a932a14480d9e435369386b02fb7964156c3969691354e76a8787edce662621d`,
   started `2026-11-24T13:07:06Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. Explicit
   `NOT isread:true` and equivalent boolean
   restrictions are accepted as exact phrase-OR alternatives. Deployed as
   `deploy-2026-11-24-eas-kql-not-boolean-or-47d9b33`; homelab commit
   `c691d81`, image
   `sha256:f3bb37414b903c7c3bb78e87e6fcb83ddda3f9fb40e4153df8a3d10c41788b4b`,
   started `2026-11-24T12:58:40Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Regression suite: 333 passed, 73 ignored; strict Clippy,
   formatting, and diff checks passed. Compound phrase AND NOT phrase
   (including unary-minus
   negation) is deployed as
   `deploy-2026-11-24-eas-kql-negated-phrase-grouping-0b53db8`; homelab commit
   `ab695ba`, image
   `sha256:6e0904559ecb893323dbd6c60615e09bb314101ade29cd41da041e8f8ed56a62`,
   started `2026-11-24T12:53:14Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The full suite passed 333 tests (73 ignored); strict
   Clippy, formatting, and diff checks passed. The previous negated-phrase feature tag before the
   same-property grouping guard is
   `deploy-2026-11-24-eas-kql-negated-phrase-and-db13a97`; it was not deployed.
   The preceding positive-boolean-OR release was deployed as
   `deploy-2026-11-24-eas-kql-phrase-negated-bool-a0304e8`; homelab commit
   `9749ba1`, image
   `sha256:2d4aaca2cc911156f3ebc69807449ac4ab733d624ca2bb215f7a9623f19026f8`,
   started `2026-11-24T12:35:18Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The prior multi-phrase mixed OR release was deployed as
   `deploy-2026-11-24-eas-kql-phrase-mixed-multi-or-11ee294`; homelab commit
   `92c0d22`, image
   `sha256:bd7c5c29e327c7352add810fd40c792825a77a0da9d5c43923514ac29d07d4dd`,
   started `2026-11-24T12:28:43Z`, restart count zero, and health/readiness
   passed. The preceding mixed phrase/boolean OR release was
   `deploy-2026-11-24-eas-kql-phrase-multi-or-225027a`; homelab commit
   `be7245f`, image
   `sha256:4bce35ec72192e121b71d217de16c874068b9b0733efa7f836107e4942660ab3`,
   started `2026-11-24T12:23:29Z`, restart count zero, and health/readiness
   passed. The initial boolean/phrase OR support was deployed as
   `deploy-2026-11-24-eas-kql-phrase-boolean-or-9ee6fb0`; homelab commit
   `815e085`, image
   `sha256:f47da35f7aacbfe826fc23d9008c649ce86605bee764fcff648eb30e261c0361`,
   started `2026-11-24T12:16:32Z`, restart count zero, with health and
   readiness passing. The initial phrase/read OR support was deployed as
   `deploy-2026-11-24-eas-kql-phrase-read-or-c07ded1`; homelab commit
   `3a88b30`, image
   `sha256:fcca8355ec570fe7af2214c196470b4864688d720699809024611e19b7852ca4`,
   started `2026-11-24T12:10:15Z`, restart count zero, with health and
   readiness passing. The initial standalone NOT support was deployed as
   `deploy-2026-11-24-eas-kql-negative-phrase-26627d7`; homelab commit
   `1a0de67`, image
   `sha256:51ec7da6f0293dfd841016ed06aa75e1d9f37bd03b5244190e42b046526d3670`,
   started `2026-11-24T11:58:13Z`, restart count zero, and health/readiness
   passed. Unary-minus support is deployed as
   `deploy-2026-11-24-eas-kql-negative-qualifier-2290936`; homelab commit
   `7697cbc`, image
   `sha256:c526ee46cdbddb73d74d13b8fbce2ead1e605a894325123ef873ab6daf242530`,
   started `2026-11-24T12:03:16Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The prior same-property AND release was
   `deploy-2026-11-24-eas-kql-repeat-property-e1a7d86`; homelab commit
   `5b8a529`, image
   `sha256:ac8282f1f2104983c0dcfb9b5e41b439bcbd0cbeab9494fffc406ee80787be7c`,
   started `2026-11-24T11:50:37Z`, restart count zero, with health and
   readiness passing.
   Positive phrases can be combined with KQL AND terms. Multiple phrases on
   distinct properties (for example subject plus body) are exact-filtered
   together; candidate filtering preserves the other restrictions, result
   order, totals, and range paging. An OR expression made only of positive
   phrase terms is exact-filtered across its alternatives. A phrase may also
   be ORed with `isread`, `isflagged`, `hasattachment(s)`, importance, received,
   sent, and size restrictions. Other mixed OR terms that lack a safe candidate
   filter or exact local matcher remain unsupported. Repeated same-property
   phrases are locally OR-grouped when their KQL grouping is unambiguous,
   including adjacent alternatives combined with supported filters and explicit
   groups. Ambiguous repeated-property forms remain rejected. Regression
   coverage checks repeated subject phrases across grouped branches with body
   and read predicates, plus exact end-to-end result totals.
   Negated phrases can also combine with exact single-token string restrictions
   on subject, body, text, from, to, cc, bcc, and participants, plus attachment
   filename restrictions; these forms broad-scan the selected mailbox and
   evaluate each restriction locally. Regressions check attachment-name
   alternatives; an end-to-end Find test confirms a negated subject phrase plus
   a positive single-token subject restriction returns the exact result and
   total.
   Other predicates outside the supported boolean, importance, received, sent,
   size, and string-property set remain rejected when paired with negated
   phrases.
   The KQL `ALL(...)`, `ANY(...)`, and `NONE(...)` free-text operators can
   also combine with exact phrase expressions; their terms are locally
   evaluated with AND, OR, and negated-OR semantics during the scoped mailbox
   scan. A mixed `WORDS(...)` synonym group also locally enforces quoted
   phrase order instead of relying on its broader JMAP candidate filter.
   Unit coverage exercises these operators, and a mocked end-to-end Find test
   verifies exact grouped results and totals.
   A single positive `body:"multiword phrase"`
   or `text:"multiword phrase"` also exact-filters JMAP's indexed candidates;
   body and text scans fetch message bodies and reject a truncated body rather
   than return an incomplete match. Implicitly OR-grouped restrictions on a
   phrase property mixed with explicit operators are evaluated locally when
   their expression tree can be preserved; ambiguous groups remain rejected.
   Direct two-operand `NEAR` and `ONEAR` queries and flat/parenthesized Boolean
   expressions containing proximity operators now broaden to the selected
   mailbox and exact-filter token positions locally. The KQL default distance
   of eight or an explicit distance is supported; ordered searches preserve
   operand order. Flat chains of proximity operators preserve MS-KQL's
   left-to-right associativity. Parenthesized proximity expressions can appear
   on either side of another proximity operator and are evaluated recursively
   against token spans in each indexed text field. Parenthesized free-text OR
   groups may also be used as proximity operands (for example,
   `cat NEAR(0) (cat OR dog)`), including NEAR's same-token match. Regression
   coverage checks nested ordered matching, OR alternatives, same-token
   matching, reversed span order, and `NOT` over a parenthesized proximity
   expression, consistent with the [MS-KQL NEAR
   operator](https://learn.microsoft.com/en-us/openspecs/sharepoint_protocols/ms-kql/31e6a96d-8b5d-4d3b-b4fa-af4b58e90a39).
   Find paging sessions now also bind the original KQL text, so queries that
   share a broadened JMAP candidate filter cannot reuse each other's page
   state. An end-to-end paging regression confirms that reusing a SearchId for
   a different proximity query returns the required restart status even though
   both queries share the same broad JMAP filter.
   Proximity
   matching also evaluates attachment filenames;
   attachment payload text remains unavailable for local proximity checks.
   MS-KQL defines `attachment:` as matching attachment file names ([Exchange
   attachment search property](https://learn.microsoft.com/en-us/exchange/policy-and-compliance/ediscovery/message-properties-and-search-operators));
   the current exact filename matcher implements that restriction.
   Unqualified free-text and proximity searches can also cover attachment
   contents. Stalwart's JMAP
   `text` filter searches extracted attachment text only when `attachment` is
   enabled in the Search object's `indexEmailFields`; its `body` filter
   deliberately excludes attachments. The aggregate `text` result contains no
   attachment token positions, so it cannot by itself implement exact NEAR or
   ONEAR distance checks. Stalwart documents extraction for text/plain,
   text/html, common Office/OpenDocument formats, EPUB, RTF, and PDF, with
   configurable size/decompression limits and partial-index cases ([Stalwart
   attachment text extraction](https://www.stalw.art/docs/storage/text-extraction/)). A
   Find now downloads and locally extracts `text/*`, JSON (including vendor
   `+json` media types), XML (`+xml`),
   RFC 822 `message/rfc822` and `message/global` attachments (including decoded
   plain/HTML MIME bodies and embedded-message subjects), Outlook
   `application/vnd.ms-outlook` messages (subject, plain body, and HTML body),
   text-bearing PDFs plus mixed text/scanned PDFs with at most three image-only
   pages,
   EPUB XHTML, OOXML Word/Excel/PowerPoint (including
   macro-enabled variants), and OpenDocument Text, Spreadsheet, and Presentation
   attachment containers up to 8 MiB for exact text/NEAR checks, with extracted
   text still capped at 4 MiB. Explicit textual MIME aliases for CSV, NDJSON,
   YAML, and TOML use the same charset-aware bounded extraction path. Legacy
   `.xls` worksheets,
   `.ppt` streams, and Word `.doc` piece tables are also parsed within explicit
   limits. Generic ZIP extraction selects text/config extensions only and
   retains the existing 4096-entry and cumulative 2 MiB expanded-data limits.
   A mocked 16.1 Find test verifies exact proximity search through a downloaded
   ZIP README. Deployed as `deploy-2026-11-24-eas-find-text-zip-4d39e9d`;
   homelab commit `88155ae`, image
   `sha256:c6dba04bc36283b834cf5ac8b025a90650339689309f4de5d67ee2a36cef48c0`,
   started `2026-11-25T00:21:08Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`; the OCR tools remain present and protocol default remains
   14.0. Text
   attachments honor MIME
   `charset` parameters, including common
   legacy single-byte encodings and UTF-16LE, with BOM detection when the
   charset parameter is absent; XML text also honors its encoding declaration.
   CSV, NDJSON, YAML, and TOML application MIME aliases use this same bounded
   decoding path; mocked 16.1 Find verifies exact proximity matching in a
   downloaded YAML attachment. The full suite passes 399 tests (74 ignored);
   strict Clippy, format, and diff checks pass. Deployed as
   `deploy-2026-11-24-eas-find-structured-text-77eb734`; homelab commit
   `6ddcb84`, image
   `sha256:eff95709a9c233b86b10c153d77f1149db25f0e0cfffee0c4449f693136f36e5`,
   started `2026-11-24T23:50:05Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
   PDFs with no selectable text fall back to Poppler rendering and English
   Tesseract OCR. Rendering probes at most four pages to detect overflow and
   only accepts PDFs with three or fewer pages, scales each page to at most
   1600 pixels per edge, applies subprocess time limits, and keeps OCR output
   under the 4 MiB text budget. The runtime image includes `poppler-utils` and
   Tesseract's English data. A generated image-only PDF fixture passes the
   real OCR path on this host; a fake-tool regression verifies page and render
   bounds without requiring system OCR packages. Multi-page mixed PDFs remain
   unsupported because partial OCR would make negative Find predicates
   inexact. Deployed as `deploy-2026-11-24-eas-find-pdf-ocr-1cf7b0c`;
   homelab commit `15cbe0d`, image
   `sha256:b2057b47548ce45c113552e245fa70d62a89d4da22cb8f34c48755d450e686d8`,
   started `2026-11-25T00:12:41Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Runtime checks confirmed `/usr/bin/pdftoppm`,
   `/usr/bin/tesseract`, and the `eng` trained data. The protocol default
   remains 14.0.
   Extracted and normalized text output is capped at 4 MiB, including bounded
   streaming output from the PDF parser and XML entity/CDATA decoding. RFC 822 and
   Outlook message decoding are capped at the same 4 MiB extracted-text limit;
   compressed Outlook RTF is not expanded. OOXML extraction includes Word
   comments and glossary text, Excel comments, and PowerPoint comments within
   the same bounded XML budget. Mocked 16.1
   Find regressions exercise quoted-printable EML extraction through download
   and exact proximity matching, exact subject matching against a real OLE
   Outlook message fixture, `application/problem+json` extraction, Windows-1252
   and UTF-16LE text decoding (including BOM-only UTF-16LE), and Windows-1252
   XML declaration decoding.
   PDF parsing failures, output-limit errors, and parser panics are contained as
   non-matches. XML output is stopped as soon as its decoded text limit is
   reached, and normalization avoids a per-word temporary allocation. Regression
   coverage checks exact-limit XML and normalization, plus rejection above the
   limit. OOXML comment extraction is checked with archive fixtures and an
   end-to-end Word comment proximity search. The full suite passes 399 tests
   (74 ignored); strict Clippy, format,
   and diff checks pass.
   Legacy `.xls` attachments are parsed with a vendored Calamine patch that
   caps dense worksheet and formula ranges at 250,000 total cells before range
   allocation; oversized coordinates fail closed. A real `.xls` fixture and a
   malicious full-grid Dimensions declaration are covered by unit tests, and
   an end-to-end 16.1 Find regression checks exact NEAR matching on workbook
   cells. Deployed as `deploy-2026-11-24-eas-find-xls-docker-39ae56e`; homelab
   commit `ed20db3`, image
   `sha256:cb9c0c0b70332bc87b2bdc158251eb9ae112282becec68927bf19d6a4932de6b`,
   started `2026-11-24T22:40:09Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Archive entry count and cumulative selected XML expansion
   are bounded. OOXML Word, Excel, and PowerPoint comment text extraction is
   deployed as `deploy-2026-11-24-eas-find-ooxml-comments-4e86021`; homelab
   commit `f04aa52`, image
   `sha256:ecfe6bd9f465b98eacbc759745296a93dc78175d3e57f2bf46fbd19e03a8271e`,
   started `2026-11-24T22:47:01Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
   Legacy binary PowerPoint slides, notes, and comments are extracted with
   `duke-slides` using limits for input bytes, CFB entries, stream bytes, record
   depth, and retained text. Unit coverage checks a generated legacy OLE PPT,
   an oversized input, and a mocked 16.1 Find proximity query. The mocked
   end-to-end test also searches a DOCX package larger than 2 MiB while keeping
   its extracted text small; attachment input is capped at 4 MiB and extracted
   text at 2 MiB. Deployed as
   `deploy-2026-11-24-eas-find-ppt-e4952a3`; homelab commit `552b5d1`, image
   `sha256:ab1a7d83f0e0bcb3cffa477af463bd5f59330638b7906bce8a79f6664c571cda`,
   started `2026-11-24T23:03:55Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Extraction remains subject to the 4 MiB input and 2 MiB
   text limits.
   Legacy binary Word documents are parsed from bounded CFB streams and their
   CLX piece tables; invalid offsets, oversized stream declarations, malformed
   OLE files, and parser panics fail closed. A generated fixture exercises
   Windows-1252 piece decoding and exact proximity matching through mocked 16.1
   Find, including a malicious stream length. Deployed as
   `deploy-2026-11-24-eas-find-doc-0001c24`; homelab commit `81e0d7a`, image
   `sha256:362e0ab3b9f8cf783641c22be87d5a4f4f6e8ab3f26cceb8ceedf1219c189ecc`,
   started `2026-11-24T23:19:04Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The attachment download cap was then raised to 4 MiB
   while retaining the 2 MiB extracted-text cap. A mocked 16.1 Find regression
   searches a DOCX package above 2 MiB, and checks that a 4 MiB + 1 attachment
   is skipped before blob download. Deployed as
   `deploy-2026-11-24-eas-find-4mb-f0dd1ac`; homelab commit `cc074c9`, image
   `sha256:d535105047f2ce2ebe1b22d4ad9ae43618bc67fb8052ba2700544e9ffbfbf860`,
   started `2026-11-24T23:41:01Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
   EPUB XHTML chapters use the same archive-entry and expanded-XML limits; a
   mocked HTTP Find regression verifies proximity matching in EPUB text. The
   macro-enabled OOXML types use the same bounded document-part selection; a
   mocked Find regression covers an `.xlsm` attachment. RFC
   822 extraction is deployed as `deploy-2026-11-24-eas-find-rfc822-e5c4f0f`;
   homelab commit `1aff02a`, image
   `sha256:8b763e9e24545d2b8779f54f816ec792c0d758ccca38db9bb807626e80d05d33`,
   started `2026-11-24T20:54:20Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Outlook `.msg` extraction is deployed as
   `deploy-2026-11-24-eas-find-outlook-msg-570285e`; homelab commit `25d23c4`,
   image `sha256:b45a5d0cb91c6d317d18d55f5bbc371a19261ea4bf35691a2c03558a41fd0ffa`,
   started `2026-11-24T21:04:00Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Vendor `+json` extraction is deployed as
   `deploy-2026-11-24-eas-find-json-c86d070`; homelab commit `be4a802`, image
   `sha256:95ead6cd7c7c8d50f43eb6dbeff6fc991d6a2d5a7c2feb94975f991e8c11ab4d`,
   started `2026-11-24T21:09:38Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. MIME charset decoding is deployed as
   `deploy-2026-11-24-eas-find-charsets-17cec20`; homelab commit `b7210c4`,
   image `sha256:bbd3cb22b2d0436c66762d217562ae31230c2234cf3ace9f38a963bd126899cb`,
   started `2026-11-24T21:31:01Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. BOM-aware MIME text decoding is deployed as
   `deploy-2026-11-24-eas-find-text-bom-4fc21a6`; homelab commit `b29b8ef`,
   image `sha256:85658da4780b72fb432fd3d48313afd0baf06379f3a6de891b53204d17c00324`,
   started `2026-11-24T21:39:18Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. XML declaration charset decoding is deployed as
   `deploy-2026-11-24-eas-find-xml-encoding-d3562d0`; homelab commit `fdc24ad`,
   image `sha256:03d6ecbe63bf63feac91ecf9375599ea33b750c6c4e54b05a5e4b0f3c7986328`,
   started `2026-11-24T22:12:57Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Bounded Find output for PDF extraction, XML decoding,
   and normalization is deployed as
   `deploy-2026-11-24-eas-find-output-bounds-89458dc`; homelab commit `81bcebc`,
   image `sha256:83597d0280f41620ef9d52370fe093a038705c00b6fa860cad897d9cf3f7de3f`,
   started `2026-11-24T22:23:53Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. EPUB
   extraction is deployed as `deploy-2026-11-24-eas-find-epub-55daf09`;
   homelab commit `97d913d`, image
   `sha256:8aa204bddf21960de0338ff595f81ce66564bf5c4015940521b5fa15730e6b23`,
   started `2026-11-24T19:54:28Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Macro-enabled OOXML extraction is deployed as
   `deploy-2026-11-24-eas-find-office-macro-6050b9f`; homelab commit
   `284dced`, image
   `sha256:c6d40e2a1d3e32d6e6be684c0d86571cb385c08cf19de32938c8f42b5d1dd63a`,
   started `2026-11-24T19:59:42Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
   HTML/RTF conversion and XML element/entity/CDATA handling are covered as
   well. The downloader enforces the cap from the actual body size even if
   attachment metadata advertises less; mocked end-to-end regressions confirm
   PDF and DOCX text matching and oversized-content rejection. Deployed as
   `deploy-2026-11-24-eas-find-office-docs-637a508`; homelab commit
   `d4bd5ac`, image
   `sha256:6b48329c78265129eae9ab73bb478d51b2465b0198465ffb2881a155edd8261b`,
   with zero restarts and passing runtime health/readiness checks.
   text-property searches broad-scan the selected mailbox so those attachment
   checks do not depend on JMAP's attachment index setting. Legacy binary Word,
   Excel, and PowerPoint formats, scanned-PDF OCR, and larger attachment
   extraction remain unsupported, so
   attachment-content proximity is still incomplete for those formats. Boolean
   combinations use the existing exact matcher for supported predicates.
   Mocked HTTP end-to-end coverage verifies both a `text:` phrase and NEAR
   against a downloaded text attachment, alongside phrase operands, direction,
   distance, proximity AND/OR combinations, exact totals, and range paging.
   Attachment-content positions remain unavailable for unsupported formats
   and files over the extraction size limit. `attachment:` terms
   and phrases also match contiguous words in attachment filenames with exact
   local post-filtering; trailing `*` performs a final-token prefix match (and
   `attachment:*` matches any non-empty attachment list) per
   [MS-KQL property restrictions](https://learn.microsoft.com/en-us/openspecs/sharepoint_protocols/ms-kql/1b4eb34a-0e20-4aca-b38e-41f531a747ff).
   Unordered NEAR also accepts both operands matching one
   indexed token, as MS-KQL specifies. `WORDS(...)` now accepts quoted
   multiword synonym operands, matching MS-SEARCH's specified equivalence
   between a word and a phrase alternative; the candidate query remains an OR
   of JMAP text terms and the local matcher enforces the quoted phrase's word
   order and adjacency. As with proximity, exact local matching cannot inspect
   extracted attachment payload text; that indexing limitation remains open.
   `WORDS(...)` also accepts trailing-prefix terms and phrases; its synonym
   alternatives are locally OR-matched with the same exact prefix/phrase rules.
   See [MS-SEARCH Synonyms](https://learn.microsoft.com/en-us/openspecs/sharepoint_protocols/ms-search/3e8338e0-d5fe-4a9f-ada9-85bc292d4c1a) and the [KeyQL wildcard reference](https://learn.microsoft.com/en-us/sharepoint/dev/general-development/keyword-query-language-kql-syntax-reference).
   Free-text and supported string-property terms now accept KQL's trailing `*`
   prefix operator, including supported AND/OR combinations with boolean
   predicates. Quoted property phrases can use it on the final phrase token.
   `NEAR` and `ONEAR` operands also preserve trailing prefix operators and
   match token starts locally, so ordered and unordered distance checks use
   the same prefix semantics as ordinary KQL terms. Regressions exercise
   prefix operands in both operators, including reversed order and a
   non-match. The full suite passes 383 tests (73 ignored); strict Clippy,
   formatting, and diff checks pass. Deployed as
   `deploy-2026-11-24-eas-kql-proximity-prefix-4cd2896`; homelab commit
   `374fb9b`, image
   `sha256:498c8fca0dc611fdec6c58da0c8bbf6c053cd396f29a6f6491d6763bd9b8337a`,
   started `2026-11-24T19:47:27Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. String-property `*` restrictions now test whether the
   selected value exists, and `NOT property:*` inverts that exact test. Empty
   truncated bodies fail closed when the result cannot be known. Unit and HTTP
   Find regressions cover the positive and negated forms. The full suite passes
   386 tests (73 ignored); strict Clippy, formatting, and diff checks pass.
   Deployed as `deploy-2026-11-24-eas-kql-property-presence-338f10a`;
   homelab commit `6107c09`, image
   `sha256:fbebbdab42bb34790b64edfe6b452e486c71ebfac1c040cc0f2424238452a6dc`,
   started `2026-11-24T20:09:07Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
   These queries use a scoped mailbox scan and exact local token-prefix
   comparison because JMAP text filters cannot preserve prefix semantics. A
   property-qualified prefix can also be negated with `NOT` or unary minus and
   is checked exactly during the same scoped scan, including supported mixed
   Boolean expressions. A mocked end-to-end Find regression checks the
   negated prefix and its boolean OR branch against exact IDs and totals.
   See the [KeyQL wildcard reference](https://learn.microsoft.com/en-us/sharepoint/dev/general-development/keyword-query-language-kql-syntax-reference).
   Regression suite: 382 passed,
   73 ignored; strict Clippy, formatting, and diff checks passed. The
   prefix-negation, compound text-operator, and attachment extraction changes
   are deployed in `deploy-2026-11-24-eas-find-office-docs-637a508`; homelab
   commit `d4bd5ac`, image
   `sha256:6b48329c78265129eae9ab73bb478d51b2465b0198465ffb2881a155edd8261b`,
   started `2026-11-24T19:25:30Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Runtime health is verified; real-client 16.1
   certification remains open.
   Phrase matching is case-insensitive and requires adjacent
   words in order. Regression tests cover plain, HTML, and RTF bodies,
   subject/body conjunctions, truncation, and unsupported Boolean forms. Full
   suite: 333 passed, 73 ignored; strict Clippy, formatting, and diff checks
   passed. Deployed as
   `deploy-2026-11-24-eas-kql-phrase-or-05bbe4b`; homelab commit
   `4de6cf1`, image
   `sha256:9c7aada5cdb22c4b5de0f846b1273184ab5995be3a26ad9d83e588fd816b64b1`,
   started `2026-11-24T11:40:44Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
   The prior AND-only multi-property phrase release was deployed as
   `deploy-2026-11-24-eas-kql-multiple-phrases-858f5cd`; homelab commit
   `0e98f88`, image
   `sha256:5695256946e4ff707d107015dfaf1688be25c7202d127723162c89ae456d736f`,
   started `2026-11-24T11:33:47Z`, restart count zero, and both health and
   readiness checks passed.
   The earlier single-property body/text phrase release was deployed as
   `deploy-2026-11-24-eas-kql-body-text-phrases-06b7292`; homelab commit
   `ee54715`, image
   `sha256:22d72a3eccdb0063926874fc4a88d477c0ca9b4535d2ff5efe7fd4f5f90ab642`,
   started `2026-11-24T11:07:15Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
   Regression coverage includes reversed/intervening words and a phrase
   combined with `isread:true`. Deployed as
   `deploy-2026-11-24-eas-find-subject-phrase-and-e8e0243`; homelab commit
   `72c8f33`, image
   `sha256:fccac4d63e2e4be61dcdd48558c88206dbad5daf6fba3b31df2f06a326bb6b61`,
   started `2026-11-24T10:10:14Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The suite passed 315 tests (73 ignored); strict Clippy,
   formatting, and diff checks passed.
   Deployed as `deploy-2026-11-24-eas-find-exact-subject-phrases-2727f06`;
   homelab commit `933b23b`. The forced Compose build produced image
   `sha256:58b834bd07b01ca9c5a1c8b9dbde8ed6253aef9da5197deeff6704d49c2e258c`.
   The container started at `2026-11-24T07:53:46Z` with restart count zero;
   `/healthz` returned `ok` and `/readyz` returned `ready`. KQL
   `importance:low|normal|high`
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
2. **Recurring series exception coverage.** Common event and task recurrence
   rules map to Stalwart's singular JSCalendar `recurrenceRule`. EAS 16.0/16.1
   occurrence deletes and subject, plain/HTML body, location/time, all-day,
   availability, category, reminder, and attendee-list edits map through
   `InstanceId` and `recurrenceOverrides`; exclusions and those fields round-trip.
   Attendee exceptions replace the occurrence roster while
   preserving the organizer and participant metadata, and read back status and
   optionality. Reminder overrides preserve unrelated alerts and support
   clearing the occurrence reminder. EAS 16.0/16.1 series rule or start/end changes
   clear existing `recurrenceOverrides` in the same state-guarded update.
   Start/end edits convert through the parent event's IANA timezone, preserve
   the inherited endpoint when only one is changed, derive the new duration,
   and reject negative durations. All-day flags are occurrence-specific.
   Sensitivity is not mapped inside recurrence overrides because RFC 8984
   prohibits `privacy` patches there. DisallowNewTimeProposal is read-only and
   maps Stalwart's retained `X-MICROSOFT-DISALLOW-COUNTER` iCalendar property;
   client writes fail closed because JSCalendar has no standard property for
   this Exchange setting.
   Top-level and recurring-exception AppointmentReplyTime values are mapped
   from the authenticated participant's unique `scheduleUpdated` value.
   Read-only
   linked attachments on recurring exceptions now map from JSCalendar enclosure
   links and are fetched from the Calendars account; EAS 16.0/16.1 only. EAS
   forbids Attachment elements in client command requests, so this is intentionally
   read-only. Enclosure links without a JMAP blobId fail closed; arbitrary hrefs
   are not fetched. ResponseRequested maps to the JSCalendar participant
   `expectReply` value when all invitees share one setting; mixed settings are
   omitted because EAS exposes a single boolean. The EAS Tasks class has no
   per-instance exception container, so Task exceptions are not an EAS gap.
   HTML descriptions now round-trip through `descriptionContentType` as well
   as the string description. Regression tests cover HTML reads and the guarded
   JMAP write. Full suite: 330 passed, 73 ignored; strict Clippy, formatting,
   and diff checks passed. Deployed as
   `deploy-2026-11-24-eas-calendar-exception-html-gate-8cf107d`; homelab
   commit `8e3e8e8`, image
   `sha256:5377ce5b3f418fade1f5b1a256dac4a7428104b30e2982ead318e295e07ba346`,
   started `2026-11-24T11:23:36Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Location support deployed as
   `deploy-2026-11-24-eas-calendar-instance-location-fb4eb72`; homelab commit
   `8e9e20d`. The forced Compose build produced image
   `sha256:42a88cd789681e1743bd1c7f5368e87f61559a0072e8b74cae236236b5eeb277`.
   The container started at `2026-11-24T08:02:34Z` with restart count zero;
   `/healthz` returned `ok` and `/readyz` returned `ready`. Time and all-day
   overrides are deployed as `deploy-2026-11-24-eas-calendar-instance-time-b5dbff2`;
   homelab commit `c53e767`. The forced Compose build produced image
   `sha256:dd5981dbf3696e21b935a5e7cb292adfa67a8a4d2707329535a15b81f7ef70df`.
   The container started at `2026-11-24T08:17:03Z` with restart count zero;
   `/healthz` returned `ok` and `/readyz` returned `ready`. Availability and
   category overrides are deployed as
   `deploy-2026-11-24-eas-calendar-instance-metadata-7f07cd4`; that source tag
   also included a `privacy` override, which this newer standards correction
   removes. Homelab commit
   `74169dc`. The forced Compose build produced image
   `sha256:dd5981dbf3696e21b935a5e7cb292adfa67a8a4d2707329535a15b81f7ef70df`.
   The container started at `2026-11-24T08:17:03Z` with restart count zero;
   `/healthz` returned `ok` and `/readyz` returned `ready`. Regression suite at
   that release: 303 passed, 73 ignored; strict Clippy passed. Reminder overrides
   and the standards correction for unsupported sensitivity are now deployed as
   `deploy-2026-11-24-eas-calendar-instance-reminder-276413d`; homelab
   commit `21b831f`, image
   `sha256:84d723522d1591ee5ff6d2f084e41a3d84b33b597f91963816f51cf64d9dc961`,
   started `2026-11-24T08:40:13Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The suite passed 304 tests (73 ignored); strict Clippy,
   formatting, and diff checks passed. The series exception reset is deployed as
   `deploy-2026-11-24-eas-calendar-series-exception-reset-c336eb1`; homelab
   commit `f0fba96`, image
   `sha256:bb2a35dab9b299af820ff0d9a207ef1663a05befda56767091cb820e4fcb93f8`,
   started `2026-11-24T08:47:51Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. Its suite passed 306 tests (73 ignored); strict Clippy,
   formatting, and diff checks passed.
   Occurrence attendee overrides are deployed as
   `deploy-2026-11-24-eas-calendar-instance-attendees-253ae5a`; homelab
   commit `d3b7253`, image
   `sha256:e2518cc278eeaf6b154383fe7b2e12168c19ae2d5c092013ede37fe94429f10d`,
   started `2026-11-24T08:58:47Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The suite passed 307 tests (73 ignored); strict Clippy,
   formatting, and diff checks passed. ResponseRequested now maps to uniform
   attendee `expectReply` settings and is deployed as
   `deploy-2026-11-24-eas-calendar-response-requested-e989c62`; homelab commit
   `61b6a67`, image
   `sha256:f14c1c672260d6fcb07bb3af41e926a6e762405dce884ca2db1b977b7c1a4efc`,
   started `2026-11-24T09:11:12Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The suite passed 310 tests (73 ignored); strict Clippy,
   formatting, and diff checks passed.
   Read-only recurring-exception attachments are deployed as
   `deploy-2026-11-24-eas-calendar-exception-attachments-ea4467e`; homelab
   commit `de2e4bc`, image
   `sha256:509c23fbc4f2f5baa504d97cad52e628a5aa40219228d20e381643a52d6cd04a`,
   started `2026-11-24T09:27:06Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The suite passed 311 tests (73 ignored); strict Clippy,
   formatting, and diff checks passed. The exception link patch supports full map
   replacement and individual nested changes; external URL-only enclosures are
   deliberately not downloaded.
3. **Calendar Timezone.** Calendar reads now preserve the JSCalendar IANA
   `timeZone` and serialize the MS-ASDTYPE 172-byte `TIME_ZONE_INFORMATION`
   structure for EAS, including recurring standard/daylight transition rules
   derived for the event year. Fixed-offset zones emit zero transition dates;
   invalid zones are omitted. Deployed as
   `deploy-2026-11-24-eas-calendar-timezone-year-77d3077`; homelab commit
   `0770b75`, image
   `sha256:5d1a1060c9e1fee06554919ac21e7c2943085bf81ec7ebab2c56c6367a6cd3f2`,
   started `2026-11-24T09:43:37Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The suite passed 312 tests (73 ignored); strict Clippy,
   formatting, and diff checks passed. Binary-layout regressions cover Toronto
   daylight transitions and fixed UTC; transition rules use the event's local
   year, including events whose UTC and local dates fall in different years.
4. **Contact notes.** Contacts Sync now reads and writes JSContact `notes`
   through AirSyncBase:Body, including the selected EAS BodyPreference,
   truncation size, and truncated flag. Writes use a dedicated `eas_notes`
   note key and preserve other JMAP note entries under the existing state
   guard. HTML notes are rejected because a plain JSContact note cannot retain
   the original markup. Deployed as
   `deploy-2026-11-24-eas-contact-notes-b58685a`; homelab commit `4e50b0d`,
   image
   `sha256:fb4c623a0c98eb42c7602fd2321112d79388bbf3db5ef0ad4be350635dc65e10`,
   started `2026-11-24T10:01:30Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`. The suite passed 314 tests (73 ignored); strict Clippy,
   formatting, and diff checks passed. The matrix also corrects already
   implemented AssistantPhoneNumber, OfficeLocation, and phonetic Yomi fields.
5. **Real-client 16.1 certification and rollout.** Production remains pinned
   to a tagged 14.0 build. Forced-version protocol tests do not establish that
   iOS/Outlook negotiate or behave correctly against an advertised 16.1 build.
   Keep 16.1 unadvertised on production until the implementation and a fresh
   device pass are complete.
6. **Legacy/low-priority commands and operations.** Mail Sync Add now creates
   drafts and non-draft Email items in the requested Mailbox. ResolveRecipients,
   ValidateCert, MeetingResponse, and ItemOperations fetches for mail, contacts,
   calendars, tasks, notes, and attachment blobs are implemented. Enterprise
   IRM remains out of scope for the personal-account gateway. Search now accepts
   multiple `Store` elements in one request, evaluates each query independently,
   and returns each store's status, results, range, and total. Regression
   coverage exercises separate mailbox queries. Full suite: 339 passed, 73
   ignored; strict Clippy, formatting, and diff checks passed. Deployed as
   `deploy-2026-11-24-eas-search-multiple-stores-dfca21b`; homelab commit
   `de8c4a3`, image
   `sha256:b77ae82a99d0ba62cd7c1cf97382f14a06138db639f2dfd691a296aff71de0f7`,
   started `2026-11-24T15:22:13Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
   Mixed explicit KQL AND/OR expressions may now repeat a phrase property when
   explicit operators separate occurrences, preserving exact Boolean
   evaluation; adjacent implicit repeats remain rejected. The full suite
   passed 339 tests (73 ignored); strict Clippy, formatting, and diff checks
   passed. Deployed as
   `deploy-2026-11-24-eas-kql-repeated-property-mixed-6b14d20`; homelab commit
   `0a41d14`, image
   `sha256:147b29674bf7bab24abff1f73e83dcbe1db06b7ed9a7bccb0379f8836ee976b2`,
   started `2026-11-24T15:34:15Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
   ItemOperations Fetch now parses the optional Schema and returns only the
   requested top-level class fields for mail, contacts, calendar events, tasks,
   and notes. The full suite passed 339 tests (73 ignored); strict Clippy,
   formatting, and diff checks passed. Deployed as
   `deploy-2026-11-24-eas-itemoperations-schema-64f4f52`; homelab commit
   `db3b8f0`, image
   `sha256:2543965c1ae42b7a3ea71e12033965a978b07b444f721cd1e157f4caaa53b0f3`,
   started `2026-11-24T15:45:06Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
7. **Online meeting URI fields.** Stalwart 0.16.27's generic JSCalendar
   property handling exposes RFC 8984 `virtualLocations` even though the
   CalendarEvent-specific Rust struct does not name the property. The gateway
   now reads SIP/SIPS URIs as `OnlineMeetingConfLink` and HTTP(S) URIs as
   `OnlineMeetingExternalLink`, including recurring-instance overrides, for
   EAS 14.1/16.0/16.1. These EAS fields are response-only per MS-ASCAL and
   client writes fail closed. Regression coverage checks base and exception
   mappings, request rejection, and versioned wire output. The full suite
   passed 319 tests (73 ignored); strict Clippy, formatting, and diff checks
   passed. Deployed as
   `deploy-2026-11-24-eas-online-meeting-uris-0f5d869`; homelab commit
   `49e235f`, image
   `sha256:79ddd9a9cfdd5c76333215384e141a0b6bbb87a53d75d8a108883e0e7966e0fc`,
   started `2026-11-24T10:27:23Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
8. **AppointmentReplyTime.** JSCalendar participant `scheduleUpdated` is the
   timestamp of that participant's latest response (RFC 8984). The gateway
   now maps it only when exactly one attendee matches the authenticated user,
   normalizes it to EAS compact UTC, and rejects the response-only field in
   client changes. Recurring exceptions use their effective attendee override
   and emit the timestamp as an Exception child. Focused regressions cover
   unique-user selection, UTC normalization, occurrence overrides, response
   serialization, and inbound rejection. The series-level mapping was first
   deployed as `deploy-2026-11-24-eas-appointment-reply-time-88a0438`; this
   release extends it to recurring exceptions. Full suite: 324 passed, 73
   ignored; strict Clippy, formatting, and diff checks pass. Deployed as
   `deploy-2026-11-24-eas-recurring-appointment-reply-time-9c4fc8e`;
   homelab commit `d4909f5`, image
   `sha256:c143eb523362032a77c28d19c7bc526b898737a1e4d48013aa0cd118f781875d`,
   started `2026-11-24T10:44:28Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.
9. **DisallowNewTimeProposal.** Stalwart/Calcard retains unrecognized
   iCalendar properties in the `iCalendar.properties` extension. When the
   `X-MICROSOFT-DISALLOW-COUNTER` boolean is present, EAS now emits its value
   for protocol 14.0 and newer. Ambiguous duplicates and malformed values are
   omitted. Client changes fail closed because JSCalendar has no equivalent
   standard property and writing the Microsoft extension has not been
   verified through Stalwart's storage conversion. Regression tests cover
   read conversion, versioned response serialization, hash changes, and
   request rejection. Full suite: 327 passed, 73 ignored; strict Clippy,
   formatting, and diff checks pass. Deployed as
   `deploy-2026-11-24-eas-disallow-new-time-proposal-c01b548`; homelab commit
   `5a2f1de`, image
   `sha256:26b6d582f7f89b745a51cc2493f959723fc51d1ddb69739626d7168160597d37`,
   started `2026-11-24T10:56:01Z`, restart count zero, `/healthz` `ok`, and
   `/readyz` `ready`.

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
JMAP's standard `Importance` header filter. The original implementation
rejected comparisons; the follow-up deployment below adds exact ordered local
evaluation. Regression tests check all three levels, case normalization,
`medium` normalization, comparisons, and invalid forms. Full suite: 276 passed,
73 ignored; clippy with warnings denied, formatting, and diff checks pass.
Deployed as `deploy-2026-11-23-eas-kql-importance-b981802`; homelab commit
`5dbd061`. Compose built image
`sha256:e2db7af99256433965e60e05d732d8b963b50222b14706d82dcd21e585f018a7`.
The replacement started at `2026-11-24T05:03:33Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production
continues to advertise EAS 14.0.

Also completed (`bd30d88`): **KQL importance comparisons**. Find now evaluates
equality, inequality, and ordered comparisons against the standard low/normal/
high importance order. Ordered and negated comparisons broaden JMAP candidates
and locally filter exact values; mixed OR terms and NOT are covered end to end.
Full suite: 341 passed, 73 ignored; strict Clippy, formatting, and diff checks
passed. Deployed as
`deploy-2026-11-24-eas-kql-importance-comparisons-bd30d88`; homelab commit
`118c119`, image
`sha256:5e3d9e325a56e4aff841ab779f5207c2ac835f1f3535456a02150b630ec64ed2`,
started `2026-11-24T16:03:06Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`.

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

Also completed (`bea1387`): **EAS calendar recurrence mapping**.
The gateway parses and writes supported EAS Recurrence fields and translates
them to Stalwart's singular `recurrenceRule` property. A fresh, localhost-only
probe against Stalwart 0.16.27 verified create/get round-trip for a weekly
rule; the 1.2.0 probe confirmed the same wire shape. The original August
finding tested plural `recurrenceRules`, which is not Stalwart's property and
therefore did not establish a backend limitation. Per-instance exclusions
were added later (`7e5bec5`); field overrides, unsupported calendar patterns,
and real-client 16.1 certification remain open. Regression tests: full suite
286 passed, 73 ignored; clippy with
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

Also completed: **Task recurrence start consistency**. EAS clients may send
the series start both as top-level `StartDate`/`UtcStartDate` and inside
`Recurrence/Start`. The parser now accepts matching start values, preferring
UTC when both top-level forms are present, and still rejects a conflicting
series start. A new Sync Add → JMAP `CalendarEvent` → Sync Fetch regression
checks the recurring task fields end to end through the mocked gateway; a
focused parser regression covers both matching and conflicting dates. Full
suite: 387 passed, 73 ignored; strict Clippy, formatting, and diff checks
pass. Deployed as `deploy-2026-11-24-eas-task-recurrence-start-1e9b8cd`;
homelab commit `e5f6987`, image
`sha256:6a535f46888e0725d7af05be102eda16eb4b406876e7f5c41672e6875eb8852f`,
started `2026-11-24T20:20:42Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`. Real Stalwart and real-client Task recurrence verification
remains open.

Task recurrence parsing also enforces MS-ASTASK's narrower `Occurrences`
type: values through 255 are accepted, while 256 and larger are rejected even
though the shared Calendar recurrence parser uses a wider representation.
Regression cases cover the 255/256 boundary. The full suite passes 394 tests
(73 ignored); strict Clippy, formatting, and diff checks pass. See the
[MS-ASTASK schema](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-astask/c16a3dcb-de5b-4de9-9026-232654d0628d). Deployed as
`deploy-2026-11-24-eas-task-occurrences-8e555b2`; homelab commit `b24a4a4`,
image `sha256:f1d8259fce691e8828305a74f5bdf7420a0574f2e14efbe063b7c3270dd47b2c`,
started `2026-11-24T21:47:42Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`.

Calendar and Task recurrence responses now include the required
`CalendarType=1` (Gregorian) for monthly, monthly-by-day, yearly, and
yearly-by-day patterns. Requests accept Gregorian calendar variants and the
Gregorian `IsLeapMonth=0` value; unsupported calendar systems, leap-month
semantics, and misplaced recurrence fields fail closed. Regression tests cover
all four response recurrence types in the Calendar class, Task output, and
accepted/rejected Task inputs. Sources: Microsoft [MS-ASCAL CalendarType](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-ascal/ca68b4ac-49e8-404a-b291-e82eb25ac885)
and [MS-ASTASK CalendarType](https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-astask/f78253a6-26d7-444c-b9a6-7f9ea6afabe7).
The full suite passes 394 tests (73 ignored); strict Clippy, formatting, and
diff checks pass. Deployed as
`deploy-2026-11-24-eas-recurrence-calendar-type-500f2ac`; homelab commit
`430aedd`, image
`sha256:4e6770b9c5e6cf0945a3ef4ce767233775613ab4a304fb8a7a99c4d3965c2032`,
started `2026-11-24T21:55:05Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`.

Task objects read from JMAP now fail closed when `recurrenceRule.count` exceeds
255, so the gateway never serializes a value outside MS-ASTASK's unsigned-byte
range. The JMAP mapping regression checks that count 256 is rejected. The full
suite passes 394 tests (73 ignored); strict Clippy, formatting, and diff checks
pass. Deployed as
`deploy-2026-11-24-eas-task-read-count-limit-03da07c`; homelab commit `3bc3b87`,
image `sha256:79b5550d92a8669db1716a185e821a4a903ca114aa33da5504362aa72b183ac1`,
started `2026-11-24T22:06:51Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`.

Weekly Task recurrence responses now always include `FirstDayOfWeek` for
protocol 14.1, 16.0, and 16.1. The writer preserves an explicit JMAP value and
uses Sunday (`0`) when none is stored; protocol 14.0 omits the field because
the token is not supported there. Tests cover the fallback, an explicit Monday
value, and version gating. Source: Microsoft [MS-ASTASK FirstDayOfWeek]
(https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-astask/05d687ce-95f1-4caa-97d2-38ff41c3ad54).
The full suite passes 394 tests (73 ignored); strict Clippy, formatting, and
diff checks pass. Deployed as
`deploy-2026-11-24-eas-task-first-day-46e04af`; homelab commit `9321d03`,
image `sha256:c4cc887225dce134ae5fc5e30db907b13e2ab45371f7e0b6a67d21708bbfa68f`,
started `2026-11-24T22:01:14Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`.

Also completed: **Task Complete validation**. MS-ASTASK requires `Complete`
to be present and equal to 0 or 1. Task Add without it, invalid values, and
duplicate elements now fail before any JMAP write; partial Change may omit
the field. Parser and Sync regressions cover both accepted boolean values,
malformed/duplicate values, missing Add data, the EAS error status, and
preservation of stored tasks. See Microsoft's [MS-ASTASK Complete element]
(https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-astask/d889f302-aef2-4cc2-8b16-faa47d39fdbb).
Full suite: 390 passed, 73 ignored; strict Clippy, formatting, and diff checks
pass. Deployed as `deploy-2026-11-24-eas-task-add-requires-complete-e3f5e6a`;
homelab commit `85ea708`, image
`sha256:c7dd991c2b88e82f07c946b82c034e3447969a64f3d5978710c71559a4040ecc`,
started `2026-11-24T20:38:01Z`, restart count zero, `/healthz` `ok`, and
`/readyz` `ready`.

Also completed (`7e5bec5`): **Calendar recurrence occurrence exclusions**.
Sync Delete with a 16.0/16.1 `InstanceId` now excludes only that occurrence
through Stalwart's `recurrenceOverrides`; ordinary Sync Delete still removes
the full series. Exclusions read from JMAP are sent back using the correct
versioned EAS exception identifier and required UID. Per-instance field edits
remain rejected rather than applying to the parent series. Tests cover WBXML
parsing, UTC-to-event-local recurrence key conversion, the conditional JMAP
patch, exception serialization for 14.0 and 16.1, and unsupported override
shapes. Full suite: 298 passed, 73 ignored; clippy with warnings denied,
formatting, and diff checks pass. Deployed as
`deploy-2026-11-24-eas-calendar-instance-exclusions-7e5bec5`; homelab commit
`58b0973`. The forced Compose build produced image
`sha256:c8deced75a06600575b186db8337fb5bbea4d531415e86fc336bf6cf5c0a4a5d`.
The container started at `2026-11-24T07:24:27Z` with restart count zero;
mesh `/healthz` returned `ok` and `/readyz` returned `ready`. Production still
advertises EAS 14.0.

Also completed (`d99b3a3`): **EAS 16.0/16.1 recurrence subject and body edits**.
Sync Change with `InstanceId` can patch an occurrence's subject and plain-text
body without changing the parent series. The JMAP update is guarded with
`ifInState`; returned overrides map into EAS exception responses. Time,
location, HTML body, and other occurrence fields were initially rejected until
their mappings could preserve the stored values; later releases added those
supported fields. Tests cover the request patch, local key conversion,
read/write field mapping, and rejection of unsupported shapes.
Full suite: 300 passed, 73 ignored; clippy with warnings denied, formatting,
and diff checks pass. Deployed as
`deploy-2026-11-24-eas-calendar-instance-fields-d99b3a3`; homelab commit
`871807d`. The forced Compose build produced image
`sha256:e3b348225ec6e9be9d3f21211823025f9d6cff4862fae011adbf0f778fbadead`.
The container started at `2026-11-24T07:39:50Z` with restart count zero; mesh
`/healthz` returned `ok` and `/readyz` returned `ready`. Production continues
to advertise EAS 14.0.

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
inside WORDS are ignored as specified. In this initial implementation, property
restrictions and multiword phrases were rejected inside WORDS; the current
source status above records the later exact phrase and prefix support.
Regression tests cover comma and whitespace forms, qualifiers, and rejected
inputs. Full suite at initial implementation: 258 passed, 73 ignored; clippy,
formatting, and diff checks passed. See [MS-SEARCH Synonyms](https://learn.microsoft.com/en-us/openspecs/sharepoint_protocols/ms-search/3e8338e0-d5fe-4a9f-ada9-85bc292d4c1a).
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
| Sync: Calendar (write: add/edit/delete) | **Done** (`deploy-2026-08-25t`; recurrence `deploy-2026-11-24-eas-calendar-recurrence-bea1387`; instance fields `deploy-2026-11-24-eas-calendar-instance-fields-d99b3a3`; HTML exception body `deploy-2026-11-24-eas-calendar-exception-html-gate-8cf107d`) | `CalendarEvent/set` | `save_calendar_event`/`destroy_calendar_event` handle lifecycle and supported recurrence rules. EAS 16.0/16.1 occurrence deletes and mapped per-instance fields use `recurrenceOverrides`; sensitivity remains unsupported because JSCalendar disallows `privacy` in recurrence patches. |
| Sync: Calendar recurrence (RRULE) | **Done** (`deploy-2026-11-24-eas-calendar-recurrence-bea1387`; instance support `deploy-2026-11-24-eas-calendar-instance-fields-d99b3a3`; HTML exception body `deploy-2026-11-24-eas-calendar-exception-html-gate-8cf107d`) | Stalwart singular JSCalendar `recurrenceRule` and `recurrenceOverrides` | Common EAS daily, weekly, monthly, nth-weekday, and yearly rules map both ways. Exclusions and supported subject/plain/HTML body overrides round-trip. Non-representable recurrence patterns and sensitivity overrides remain unsupported. |
| Sync: Tasks | **Done, two-way (`deploy-2026-08-26i`); recurrence mapping (`deploy-2026-11-25-eas-task-recurrence-bdd323e`); Task validation (`deploy-2026-11-24-eas-task-add-requires-complete-e3f5e6a`) | `CalendarEvent` with `@type: "Task"` — confirmed live, not a separate JMAP object | Existing Add/Change/Delete lifecycle is live-verified over WBXML. Common EAS recurrence patterns map through the shared JSCalendar `recurrenceRule` conversion. A live E2E Add/Fetch test against isolated Stalwart verifies recurring Task creation, persisted `recurrenceRule`, and explicit WBXML Sync Fetch. Task Add requires `Complete`; it accepts only 0/1, and malformed or duplicate values fail without writing. Real-client recurrence behavior remains open. MS-ASTASK’s repeating `xs:choice` does not constrain field order; actual iOS handling remains unverified. Per-instance recurrence overrides are rejected. |
| Sync: Notes | **Done, two-way** | Email-backed synthetic-message workaround (`src/jmap/notes.rs`) | The most structurally interesting piece of this whole gateway — see that file's module doc. Full Add/Change/Delete supported. |
| FolderSync | **Done, incremental** | `Mailbox/get`, `AddressBook/get`, `Calendar/get` | Key 0 sends a full Adds snapshot; subsequent keys diff per-device collection fingerprints into Adds/Updates/Deletes. Unknown keys return Status 9. |
| FolderCreate/Update/Delete | **Done** (`deploy-2026-11-22-eas-groupware-folders-094b478`) | `Mailbox/set`, `AddressBook/set`, and `Calendar/set` | Mail folders retain hierarchical parent support. User Contacts (`Type=14`) and Calendar (`Type=13`) folders create, rename, and delete JMAP collections with synthetic `ab_`/`cal_` EAS IDs; those collections remain root-level because JMAP has no parent hierarchy. Notes and Tasks remain derived collections without independent CRUD. Mocked HTTP regression covers create/rename/delete for both types and rejects invalid types. |
| Ping | **Done** | N/A (gateway-local long-poll + JMAP poll underneath, implementation detail not deeply re-read this pass) | Empirically rock-solid over the entire multi-day A/B test; deliberately not touched further this session even where a codepage table read once looked mismatched. |
| ItemOperations (Fetch) | **Implemented (legacy/secondary path)** | `Email/get` + `download_blob()` | Multiple Fetch operations, per-operation Store dispatch, body/MIME preferences, and optional Schema field filtering are supported for mail, contacts, calendars, tasks, and notes. Real device confirmed to use the *Sync-embedded* Fetch instead for opening messages. |
| ItemOperations (attachment fetch via ItemOperations, not GetAttachment) | **Implemented** | `download_blob_with_type()` | `FileReference` fetch supports full and byte-range blob responses. ItemOperations Fetch also routes notes, tasks, calendar events, contacts, and mail. |
| GetAttachment (legacy) | **Done** | `download_blob()` | `AttachmentName` reference is a synthetic `blobId||name` string this gateway itself issues. |
| GetItemEstimate | **Done** | Counts from the same `Email/query`/`ContactCard/query`/`CalendarEvent/query` paths used for Sync | FolderType field bug fixed this session. |
| MoveItems | **Done (mail only), non-mail behavior confirmed safe** | `Email/set` (`mailboxIds`) | Mail only. Confirmed by direct code read (no longer unverified): a Notes/Contacts/Calendar item would fail cleanly with Status 5 (`Email/set` rejects the non-Email id as `notFound`), not silent corruption. Currently moot anyway — every non-mail collection has exactly one folder, so no real client has anywhere to offer moving an item to. See command-matrix doc for the full reasoning. |
| Search (mailbox + GAL) | **Done, both halves (`deploy-2026-08-26n`)** | `Email/query` `filter:{text:...}` (mailbox); `Principal/query` `filter:{text:...}` (GAL) | Both live-tested BEFORE writing any handler code, not assumed: `Email/query`'s `text` filter does real full-text matching (a probe search matched a message only in its body, not the subject); `Principal/query`'s `text` filter matches against the 7 real principals on this instance (not an empty directory) by whole-token match (a short prefix of "khuo.ng" matched nothing; the full first name "karen"/"khuong" matched exactly that principal) -- weaker than real Exchange GAL's substring/prefix ANR, but genuinely working. `search()` (`activesync.rs`) dispatches `Store>Name` to the right one; a Mailbox search restricted to any `airsync:Class` other than "Email" returns a clean empty result rather than erroring. `EAS Range`'s zero-based-inclusive "m-n" converts directly to JMAP `position`/`limit`. Real bug caught during live verification, not left in: the first pass reused `write_email_fields`'s `body_pref=None` (its own doc comment: "always full body -- correct for ItemOperations Fetch"), so a 4-result search came back as a 132KB response with every match's full HTML body and attachment metadata inlined -- fixed by parsing Search's own `airsyncbase:BodyPreference` (the identical element Sync's Options carries) and defaulting to a 500-char plain-text preview when absent, confirmed live to shrink the same 4-result response to 2.9KB. `ItemOperations`'s Fetch parser was also extended to accept a bare `search:LongId` (confirmed against the real Fetch (ItemOperations) schema as a genuine alternative to CollectionId+ServerId, not an addition) so a device can open a search result's full body afterward -- live-verified: fetched the top mailbox-search result by its LongId and got the full email back via the same `get_email_by_id` path Sync-triggered Fetches use. The separate ActiveSync `Find` command is implemented in `src/activesync/find.rs` and gated to 16.1. It remains absent from `SUPPORTED_COMMANDS` while the gateway advertises only 14.0; forced-version regressions exercise the handler, and real-client certification is still required before advertising 16.1. |
| SendMail | **Done** | `EmailSubmission/set` (blob-upload then submit) | Confirmed live and working. |
| SmartReply / SmartForward | **Done, threading fidelity confirmed** | Same `EmailSubmission/set` path as SendMail | Live-tested this session with a self-addressed message carrying a real Message-ID/In-Reply-To/References triplet: byte-identical values confirmed on both the Sent Items copy and the delivered Inbox copy. See roadmap item 4 for the full test. |
| ResolveRecipients / ValidateCert | **Implemented, end-to-end covered** | Principal/contact directory matching; local X.509 chain validation | `src/activesync/resolve_recipients.rs` and `src/activesync/validate_cert.rs`; regression coverage in `src/push_tests.rs`. S/MIME compose/signing remains separate and is not implied by these commands. |
| Settings: DeviceInformation / UserInformation | **Done (Get)** | Static/echo — no real JMAP-backed device-info storage | `PrimarySmtpAddress` bug (unrecognized field, cascading parse failure) fixed this session by removing it; `EmailAddresses>SMTPAddress` carries the same info in a valid shape. |
| Settings: Oof (Get) | **Done** *(disabled path live-verified; enabled path spec-derived, not device-verified)* | `VacationResponse/get` (`src/jmap/vacation.rs`) | Real account state now read on every Get. Disabled (`OofState=0`) shape confirmed live, unchanged from the earlier z-push comparison fix. Enabled shape's `OofMessage` block is built from MS-ASSETTINGS' own schema, not live-toggled -- see `src/jmap/vacation.rs` module doc for why (toggling `isEnabled=true` on the real account risks a genuine auto-reply going out unsupervised). Verify with a real device present before fully trusting the enabled path. |
| Settings: Oof (Set) | **Done, persists and validates** | `VacationResponse/set` (`src/jmap/vacation.rs`) | Live-verified end to end with `isEnabled=false` (safe -- confirmed via direct JMAP query that `subject`/`textBody` actually persisted to the real `VacationResponse.singleton` object, then cleaned back up). `isEnabled=true` was deliberately never live-toggled for the same reason as the Get path above. Set now accepts only OofState 0/1/2, requires StartTime and EndTime together (Status 2 otherwise), and drops schedule times for non-time-based states per MS-ASCMD. Omitted Set fields preserve their JMAP values; a ReplyMessage-only update no longer disables Oof or erases a schedule. It returns failure status 100 if JMAP write or readback fails instead of acknowledging a change that was not persisted; the Settings status specification reserves 100–255 for property-specific errors. Partial-update behavior is deployed in `deploy-2026-11-24-eas-oof-partial-b067da4` (homelab `b8be778`, image `sha256:53b9c323df006c8191a44362f91343882af38fb15f7508a4a59c51456cbdd589`, `/healthz` and `/readyz` healthy). |
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
   The gateway now implements common EAS recurrence mappings and excluded
   event occurrences. Sensitivity and other unsupported event overrides fail
   closed; Tasks have no per-instance exception container in MS-ASTASK, and
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
