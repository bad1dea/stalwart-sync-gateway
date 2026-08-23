# stalwart-sync-gateway

Experimental JMAP-native synchronization gateway for Stalwart Mail Server.

The canonical backend is Stalwart JMAP. Exchange ActiveSync is the first protocol frontend, not the internal architecture.

Current status: M1 foundation plus initial mail receive/sync. Implemented endpoints:

- `GET /healthz`
- `GET /readyz`
- `GET /metrics`
- `OPTIONS /Microsoft-Server-ActiveSync`
- `POST /Microsoft-Server-ActiveSync`
- `POST /Autodiscover/Autodiscover.xml`

`POST /Microsoft-Server-ActiveSync` authenticates against Stalwart by fetching the JMAP Session resource, decodes bounded WBXML, supports `FolderSync`, initial mail `Sync`, `MoveItems`, `GetItemEstimate`, `Settings`, and a minimal no-policy `Provision` response.

## Run

```bash
docker compose up --build sync-gateway
```

Important environment:

- `STALWART_JMAP_URL`: normally `http://stalwart:8080/.well-known/jmap`
- `EAS_PUBLIC_URL`: public ActiveSync URL returned by Autodiscover
- `STATE_BACKEND`: `memory`, `sqlite`, or `redis`; SQLite is the single-instance default
- `STATE_SQLITE_PATH`: SQLite database path, normally `/data/state.db`

## Local Test

The current local test image is:

```bash
stalwart-sync-gateway:m1-mail-sync
```

Start it against a Stalwart instance listening on the Docker host at port `8080`:

```bash
docker run -d --name stalwart-sync-gateway-test \
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
curl -i -X POST http://127.0.0.1:18080/Autodiscover/Autodiscover.xml
curl -i -u 'user@example.com:password' -X OPTIONS \
  'http://127.0.0.1:18080/Microsoft-Server-ActiveSync?User=user@example.com&DeviceId=testdevice&DeviceType=Test'
```

Initial `FolderSync` with `SyncKey=0`:

```bash
printf '\x03\x01\x6a\x00\x00\x07\x56\x52\x03\x30\x00\x01\x01' > /tmp/foldersync-0.wbxml

curl -i -u 'user@example.com:password' \
  -H 'Content-Type: application/vnd.ms-sync.wbxml' \
  --data-binary @/tmp/foldersync-0.wbxml \
  'http://127.0.0.1:18080/Microsoft-Server-ActiveSync?Cmd=FolderSync&User=user@example.com&DeviceId=testdevice&DeviceType=Test' \
  --output /tmp/foldersync-response.wbxml

xxd /tmp/foldersync-response.wbxml | head
```

`FolderSync` currently returns the full current hierarchy from JMAP every time. Mail `Sync` tracks seen Email ids per user/device/collection in the state backend so repeat requests do not replay the same Adds. Client read/unread changes, deletes, and `MoveItems` are applied through JMAP `Email/set`. Incremental hierarchy diffs and JMAP `queryChanges` are next.
