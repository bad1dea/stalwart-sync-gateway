# Handoff Notes

## Project

`stalwart-sync-gateway` is a JMAP-native synchronization gateway for Stalwart Mail Server. ActiveSync is the first protocol frontend. Stalwart/JMAP is the canonical backend.

Do not reframe this as a Z-Push clone. Z-Push and PR #187 are reference implementations and compatibility oracles only.

## Current Status

The current branch is deployable for early mail-sync testing, but it is not yet a full Z-Push replacement.

Implemented:

- Docker-native Rust service.
- Non-root runtime image.
- SQLite state backend under `/data`.
- `GET /healthz`
- `GET /readyz`
- `GET /metrics`
- `POST /Autodiscover/Autodiscover.xml`
- `OPTIONS /Microsoft-Server-ActiveSync`
- ActiveSync WBXML decoder/encoder.
- JMAP Session discovery.
- JMAP capability discovery.
- `FolderSync` from JMAP `Mailbox/get`, `AddressBook/get`, and `Calendar/get`.
- Mail receive `Sync` from JMAP `Email/query` + `Email/get`.
- Per-user/device/collection SyncKey state in SQLite.
- Duplicate Add suppression using persisted seen JMAP Email ids.
- Client read/unread via JMAP `Email/set` keyword patch.
- Client delete via JMAP `Email/set destroy`.
- `MoveItems` via JMAP `Email/set` mailbox patching.
- Minimal `Provision` no-policy response.
- Minimal `Settings` response with user email information.
- Minimal `GetItemEstimate` response.
- Bounded `Ping` compatibility response.
- Notes two-way sync (Add/Change/Delete), stored as JMAP `Email` in a `Notes`
  mailbox with a permanent random stable id carried as a `noteid-*` keyword
  (independent of the underlying JMAP Email id, which changes on every edit
  because Email is immutable). Resolved via `Email/query {hasKeyword}`. Folder
  name is the literal `Notes` (not `EAS Notes` — unify with whatever real
  Notes folder the account already has, matching the PHP z-push fix). Auto-
  creates the `Notes` mailbox via `Mailbox/set` if the account doesn't have
  one yet, so it's always advertised in `FolderSync`. See `src/jmap/notes.rs`.

Not yet implemented:

- Real JMAP `Email/queryChanges` / `Email/changes` for mail (Notes sync uses
  full-list diffing against persisted per-item state — see `item_states` in
  `src/state.rs` — which is fine at Notes' typical item counts but wouldn't
  scale to mail).
- Server-side deletions returned to clients as ActiveSync deletes (mail).
- Send/reply/forward.
- Attachment fetch/upload.
- `ItemOperations`.
- Real JMAP EventSource/WebSocket push wakeups.
- Contacts two-way sync.
- Calendar two-way sync.
- Tasks.
- Folder create/update/delete (other than the Notes mailbox auto-create above).
- Integration fixture environment with Stalwart accounts.

## Fixed Since Initial Handoff

- **apiUrl not rebased to the reachable Traefik host.** The JMAP session
  response advertises internal hostnames (e.g. `hermes.zt.khuo.ng`) that
  aren't independently routable; `session_with_basic()` now calls
  `rebase_session_urls()` to rewrite every session URL's authority onto the
  authority actually used to reach the session endpoint (same class of bug
  the PHP z-push/PR#187 fork's `rebaseSessionUrls()` fixed). See
  `src/jmap/client.rs`.
- **`rebase_one()` corrupting JMAP URI Template placeholders.** The first fix
  attempt routed the rebase through `url::Url::parse(...).path()`, which
  percent-encodes per RFC 3986 — silently turning literal `{accountId}` into
  `%7BaccountId%7D` in the rebased URL, so a later plain-string
  `.replace("{accountId}", ...)` never matched and blob upload 404'd. Fixed
  by extracting path+query via raw string slicing (`path_and_query_raw()`)
  instead of going through `Url`'s accessors, so template placeholders pass
  through byte-for-byte. Caught live via the Notes blob-upload path; would
  equally have broken `{blobId}`/`{types}` download-url substitution.

## Repository

GitHub:

```text
https://github.com/bad1dea/stalwart-sync-gateway
```

The local workspace has an unusual mounted `.git` directory. If working in this existing workspace, use:

```bash
git --git-dir=/tmp/stalwart-sync-gateway.git --work-tree=/home/khuong/dev/stalwart-jmap-gateway status
```

Fresh clones from GitHub do not need that workaround.

## Build And Test

The host may not have `cargo`; Docker is the reliable test path:

```bash
docker run --rm -v "$PWD":/app -w /app rust:bookworm bash -lc \
  'export PATH=/usr/local/cargo/bin:$PATH; rustup component add rustfmt clippy >/dev/null; cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test'
```

Build the local image:

```bash
mkdir -p /tmp/docker-config
DOCKER_CONFIG=/tmp/docker-config docker build -t stalwart-sync-gateway:m1-mail-sync .
```

Run against a Stalwart instance reachable from the Docker host on port `8080`:

```bash
docker rm -f stalwart-sync-gateway-test >/dev/null 2>&1 || true

DOCKER_CONFIG=/tmp/docker-config docker run -d --name stalwart-sync-gateway-test \
  --add-host=host.docker.internal:host-gateway \
  -p 18080:8080 \
  -e STALWART_JMAP_URL=http://host.docker.internal:8080/.well-known/jmap \
  -e EAS_PUBLIC_URL=http://localhost:18080/Microsoft-Server-ActiveSync \
  -e STATE_BACKEND=sqlite \
  -e STATE_SQLITE_PATH=/data/state.db \
  -e RUST_LOG=debug \
  -v stalwart-sync-gateway-test-data:/data \
  stalwart-sync-gateway:m1-mail-sync
```

Basic checks:

```bash
curl -i http://127.0.0.1:18080/healthz
curl -i http://127.0.0.1:18080/readyz
curl -i http://127.0.0.1:18080/metrics
```

`/readyz` returns `503` when Stalwart is not reachable. That is expected.

## ActiveSync Test Notes

Autodiscover endpoint:

```text
POST /Autodiscover/Autodiscover.xml
```

ActiveSync endpoint:

```text
/Microsoft-Server-ActiveSync
```

Manual `OPTIONS` check:

```bash
curl -i -u 'user@example.com:password' -X OPTIONS \
  'http://127.0.0.1:18080/Microsoft-Server-ActiveSync?User=user@example.com&DeviceId=testdevice&DeviceType=Test'
```

Manual initial `FolderSync` body:

```bash
printf '\x03\x01\x6a\x00\x00\x07\x56\x52\x03\x30\x00\x01\x01' > /tmp/foldersync-0.wbxml

curl -i -u 'user@example.com:password' \
  -H 'Content-Type: application/vnd.ms-sync.wbxml' \
  --data-binary @/tmp/foldersync-0.wbxml \
  'http://127.0.0.1:18080/Microsoft-Server-ActiveSync?Cmd=FolderSync&User=user@example.com&DeviceId=testdevice&DeviceType=Test' \
  --output /tmp/foldersync-response.wbxml
```

## Next Best Tasks

1. Replace seen-id duplicate suppression with JMAP `Email/queryChanges` and `Email/changes`.
2. Return server-side deletes as ActiveSync `Delete`/`SoftDelete`.
3. Implement `ItemOperations` for message and attachment fetch.
4. Implement `SendMail`, then `SmartReply` and `SmartForward`.
5. Implement JMAP EventSource/WebSocket push and wake pending `Ping` requests.
6. Build a disposable integration environment with Stalwart test accounts.
7. Start real client testing with iOS/Samsung before adding broad command surface.

Notes sync (Add/Change/Delete + self-echo suppression via `just_written`,
stable id stability across edits, folder auto-create) has been live-tested
end-to-end against a real Stalwart instance with a raw WBXML client script
and confirmed working. It has not yet been tested from a real iOS device.

## Important Constraints

- Do not log passwords, Authorization headers, tokens, mail bodies, contact bodies, note contents, or attachment bytes.
- Keep JMAP as the canonical backend.
- Do not add IMAP, SMTP, CalDAV, or CardDAV for normal backend behavior while JMAP can provide the required capability.
- Keep protocol-specific ActiveSync structures at the edge.
- Keep canonical model and JMAP layer reusable for future protocol frontends.
- Z-Push is AGPL; do not directly translate source code without preserving license implications.
