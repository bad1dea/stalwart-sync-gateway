# Handoff Notes

## Project

`stalwart-sync-gateway` is a JMAP-native synchronization gateway for Stalwart Mail Server. ActiveSync is the first protocol frontend. Stalwart/JMAP is the canonical backend.

Do not reframe this as a Z-Push clone. Z-Push and PR #187 are reference implementations and compatibility oracles only.

## Current Status (2026-08-24)

Live in production on hermes (`compose/hermes/services/stalwart-sync-gateway.yml`
in the homelab repo), fronting `eas.khuo.ng` / `autodiscover.khuo.ng`, replacing
the earlier PHP z-push trial. Mail, Notes, Contacts, and Calendar all sync real
data; sending, replying, forwarding, and attachments all work. Contacts/
Calendar/attachments are read-only (see Not Yet Implemented).

Implemented:

- Docker-native Rust service, non-root runtime image, SQLite state under `/data`.
- `GET /healthz` `GET /readyz` `GET /metrics`.
- `POST /Autodiscover/Autodiscover.xml`, `OPTIONS`/`GET`/`POST /Microsoft-Server-ActiveSync`.
- ActiveSync WBXML decoder/encoder, JMAP session/capability discovery.
- `FolderSync` from `Mailbox/get`, `AddressBook/get`, `Calendar/get`.
- **Mail**: `Sync` receive from `Email/query`+`Email/get` (seen-id dedup, not
  real `queryChanges` yet); client read/unread via `Email/set`; client delete
  via `Email/set destroy`; `MoveItems`; correct `DateReceived` (compact EAS
  DateTime, not raw ISO 8601 -- was wrong for a long time, see Fixed section);
  `AirSyncBase:Preview` (plain-text list-row snippet, HTML-stripped); correct
  `NativeBodyType` tag (was writing into the wrong WBXML field, see Fixed);
  attachments advertised (`AirSyncBase:Attachments`) and downloadable via
  `GetAttachment` (`FileReference` = `"blobId||name"`, no server-side state
  needed to resolve it).
- **Sync-embedded `<Fetch>`** (inside `<Commands>`, distinct from
  `ItemOperations`): answers with a full `<Responses><Fetch>` -- this, not
  ItemOperations, turned out to be what a real iOS client actually calls when
  opening a message from a Sync result (confirmed live). Was silently dropped
  before, which hung the message-open UI forever.
- **`ItemOperations`/`Fetch`** ("Mailbox Store" form: Store/CollectionId/
  ServerId) also implemented, for whatever client/protocol version does use it.
- **`SendMail`/`SmartForward`/`SmartReply`**: handles both the raw-MIME
  "simplified" transport real EAS 14.0+ clients use (body IS the MIME
  message, no WBXML, SaveInSentItems/ClientId as query params) and the
  older WBXML-wrapped `ComposeMail` form. Fetches the account's JMAP
  `Identity`, rewrites the `From:` display name to it, uploads the MIME as a
  blob, `Email/import`s into Sent Items, `EmailSubmission/set`s with
  `envelope: null`. Verified live: message landed in Sent Items AND was
  actually delivered.
- **Notes** two-way sync (Add/Change/Delete), stored as JMAP `Email` in a
  `Notes` mailbox with a permanent random stable id carried as a `noteid-*`
  keyword (independent of the underlying JMAP Email id, which changes on
  every edit because Email is immutable). Resolved via `Email/query
  {hasKeyword}`. Folder name is the literal `Notes` (unify with whatever real
  Notes folder the account already has). Auto-creates the mailbox if absent.
  See `src/jmap/notes.rs`.
- **Contacts** (read-only): JSContact (`ContactCard/query`+`get`) -> EAS
  Contacts fields (FirstName/LastName/FileAs, up to 3 emails, Mobile/Home/
  BusinessPhoneNumber by JSContact phone `contexts`, CompanyName, JobTitle).
  Same list-and-diff-by-seen-id shape as mail.
- **Calendar** (read-only, non-recurring events only): JSCalendar
  (`CalendarEvent/query`+`get`) -> Subject/Location/StartTime/EndTime/
  AllDayEvent/DtStamp. JSCalendar's `start` is LOCAL time + a separate IANA
  `timeZone` field (or absent = already-UTC) -- converted to real UTC via
  `chrono-tz` (the IANA tz database), not a naive fixed offset; see
  `local_to_utc_eas`/`parse_iso8601_duration_seconds` in `src/jmap/client.rs`
  and their DST-differentiating unit tests.
- Per-user/device/collection SyncKey state in SQLite; contacts/calendar
  handshake advances sync_key 0->1 on first sync even with nothing to send
  (was a real bug -- see Fixed).
- Minimal `Provision` no-policy response.
- `Settings` with `UserInformation` and `Oof` (Automatic Replies) -- `Get`
  always reports disabled (no backend wired up), `Set` is accepted but inert
  (doesn't hang/error, doesn't persist either) -- stops the client hang; real
  fix is wiring to Stalwart's ManageSieve vacation extension.
- `GetItemEstimate`, bounded `Ping`.

Not yet implemented:

- Real JMAP `Email/queryChanges` / `Email/changes` for mail (full-list diffing
  against persisted seen-ids/item-states is fine at typical item counts but
  wouldn't scale to a huge mailbox). Confirmed live this session that Stalwart
  genuinely supports both methods -- this is now a scoped, well-understood
  follow-up, not an open question about feasibility.
- Server-side mail deletions ARE now surfaced as ActiveSync deletes
  (`emails_still_in_mailbox` in `src/jmap/client.rs`) -- done this session.
- Calendar recurrence, attendees, reminders, timezone-aware all-day handling
  beyond the boolean flag.
- Attachment upload (composing a NEW message with an attachment from the
  client) -- download works, upload (part of SendMail's MIME) is whatever the
  client's own MIME construction already includes, untested with a real
  client-attached file.
- Real JMAP EventSource/WebSocket push wakeups.
- Tasks.
- Folder create/update/delete (other than the Notes mailbox auto-create).
- `MeetingResponse`, `Search`, `ResolveRecipients`, `ValidateCert`, `Find` --
  advertised in `SUPPORTED_COMMANDS` but have no handler; would 501.
- Integration fixture environment with Stalwart accounts (all verification
  so far has been live, ad hoc, against a real Stalwart instance).

## Fixed Since Initial Handoff

Roughly chronological; see git log for full detail on each.

- **apiUrl not rebased to the reachable Traefik host** -- session response
  advertises internal-only hostnames; `rebase_session_urls()` fixes this.
- **`rebase_one()` corrupting JMAP URI Template placeholders** -- routing the
  rebase through `url::Url`'s accessors percent-encoded `{`/`}` in
  `{accountId}` etc., breaking a later literal-string `.replace()`. Fixed
  with raw string slicing (`path_and_query_raw()`).
- **Contacts/calendar stuck at `sync_key=0` forever** -- the stub only
  advanced/persisted sync_key when a client command was applied; the initial
  handshake (sync_key `0`->`1`) with zero commands never did, so the server
  echoed `0` back forever and the client retried in a ~300ms tight loop
  indefinitely (confirmed live on a real account for several minutes).
- **`Email/get` deserialization crashed on any message with no Cc header**
  (i.e. almost all real mail) -- `from`/`to`/`cc` were typed as bare
  `Vec<EmailAddress>`; JMAP sends a literal `null`, not a missing key, when
  the header is absent, which `#[serde(default)]` alone doesn't cover.
  Fixed with `Option<Vec<_>>`.
- **`DateReceived` sent as raw ISO 8601** instead of compact EAS DateTime --
  wrong timestamps (client fell back to "now") on every synced message.
  `eas_datetime()` existed and was already used correctly for Notes;
  `DateReceived` just never called it.
- **Sync-embedded `<Fetch>` commands silently dropped** -- the actual root
  cause of "opening a message hangs forever with no body," discovered by
  correlating a real device's own traffic: it doesn't call `ItemOperations`
  at all, it sends `<Fetch>` inside `<Commands>`. Never answered before this.
- **`NativeBodyType` used the wrong WBXML tag** (`0x15`, which is actually
  `IsInline`) -- every message wrote a body-type constant into an unrelated
  boolean field and never sent the real NativeBodyType.
- **No `AirSyncBase:Preview`** -- list-view snippet fell back to showing raw
  HTML source verbatim, since the client had no dedicated plain-text field
  to summarize with.
- **Stalwart's own brute-force protection permanently blocked the gateway's
  Docker IP** after enough auth failures accumulated during testing
  (`expiresAt: null`, no auto-expiry) -- looked exactly like "wrong
  password"/"server crashed" from the client side. Fixed by adding the
  gateway's whole Docker subnet to Stalwart's `AllowedIp` allowlist (it's a
  trusted internal forwarder authenticating per-request on real users'
  behalf, not an untrusted external client -- fail2ban on it just self-DoSes
  the whole gateway on a single user typo).

## Repository

GitHub: `https://github.com/bad1dea/stalwart-sync-gateway` (public).

The local workspace has an unusual mounted `.git` directory. If working in this existing workspace, use:

```bash
git --git-dir=/tmp/stalwart-sync-gateway.git --work-tree=/home/khuong/dev/stalwart-jmap-gateway status
```

Fresh clones from GitHub do not need that workaround.

Deployed via `compose/hermes/services/stalwart-sync-gateway.yml` in the
**homelab** repo, `build.context` pinned to a git **tag** (`deploy-YYYY-MM-DD[a-z]`),
not `main` and not a bare commit SHA (GitHub's smart-HTTP protocol only lets
BuildKit fetch a ref that's the tip of an advertised branch/tag -- a bare SHA
fails with "repository does not contain ref" even when it's reachable).
Bump the pin, `docker compose build stalwart-sync-gateway` on hermes (Komodo/
compose won't notice a same-file build-context change), then
`up -d --force-recreate` (or plain `up -d` if other compose fields also
changed, e.g. an env var).

## Build And Test

The host may not have `cargo`; Docker is the reliable test path:

```bash
docker run --rm -v "$PWD":/app -w /app rust:bookworm bash -lc \
  'export PATH=/usr/local/cargo/bin:$PATH; rustup component add rustfmt clippy >/dev/null; cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test'
```

Build the local image:

```bash
docker build -t stalwart-sync-gateway:local .
```

Run against a reachable Stalwart instance (adjust `STALWART_JMAP_URL` to
wherever it actually is -- e.g. `http://10.99.0.10:8025/.well-known/jmap`
for hermes over ZeroTier from a laptop, or the Docker network's own
`http://stalwart:8080/.well-known/jmap` from inside the `proxy` network):

```bash
docker rm -f stalwart-sync-gateway-test >/dev/null 2>&1 || true
docker run -d --name stalwart-sync-gateway-test \
  -p 18080:8080 \
  -e STALWART_JMAP_URL=http://10.99.0.10:8025/.well-known/jmap \
  -e EAS_PUBLIC_URL=http://localhost:18080/Microsoft-Server-ActiveSync \
  -e STATE_BACKEND=sqlite \
  -e STATE_SQLITE_PATH=/data/state.db \
  -e RUST_LOG=info \
  -v stalwart-sync-gateway-test-data:/data \
  stalwart-sync-gateway:local
```

Basic checks:

```bash
curl -i http://127.0.0.1:18080/healthz
curl -i http://127.0.0.1:18080/readyz   # 503 if Stalwart isn't reachable
curl -i http://127.0.0.1:18080/metrics
```

## ActiveSync Test Notes

- `POST /Autodiscover/Autodiscover.xml`
- `OPTIONS`/`POST /Microsoft-Server-ActiveSync` (WBXML, `?Cmd=...`)
- `GET /Microsoft-Server-ActiveSync?Cmd=GetAttachment&AttachmentName=...`
  (the one command that's a plain GET, no WBXML at all)
- `POST ...?Cmd=SendMail` with `Content-Type: message/rfc822` and the raw
  MIME as the body (no WBXML) is the transport real clients actually use.

Every live-tested feature this session was verified end to end against a
real Stalwart instance (own JMAP calls + a small hand-rolled Python WBXML
encoder/decoder), not just decoded/inspected -- see git commit messages for
the specific request/response pairs checked. Notes, mail, autodiscover, and
the retry-loop/auth-block fixes have additionally been confirmed against a
real iOS device in production. Contacts/Calendar/attachments/SendMail have
NOT yet been tested from a real device as of this handoff -- next session
should start there.

## Important Constraints

- Do not log passwords, Authorization headers, tokens, mail bodies, contact bodies, note contents, or attachment bytes. (Debug-level traces added this session for diagnosing live issues are metadata-only -- ids, lengths, types, dates -- never content; keep it that way.)
- Keep JMAP as the canonical backend.
- Do not add IMAP, SMTP, CalDAV, or CardDAV for normal backend behavior while JMAP can provide the required capability.
- Keep protocol-specific ActiveSync structures at the edge.
- Keep canonical model and JMAP layer reusable for future protocol frontends.
- Z-Push is AGPL; do not directly translate source code without preserving license implications -- this session ported *behavior* (JMAP call sequences, field mappings) from the PHP reference by reading and reimplementing, not by copying code.
- WBXML codepage token numbers for Settings/ItemOperations/ComposeMail/Contacts were reconstructed from memory against the standard MS-ASWBXML numbering pattern and cross-checked live where possible; Calendar's codepage (4) has the LOWEST confidence of the set (no second source to check against) -- if a real calendar event doesn't render right on a real device, check `wbxml/eas.rs`'s `calendar` module first.
