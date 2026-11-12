# Testing

M0 unit tests cover WBXML decode and encode/decode round trips.

## Manual ActiveSync Smoke Test

With the gateway running on `127.0.0.1:18080`, create an initial FolderSync WBXML request:

```bash
printf '\x03\x01\x6a\x00\x00\x07\x56\x52\x03\x30\x00\x01\x01' > /tmp/foldersync-0.wbxml
```

Then call the gateway with a real Stalwart account:

```bash
curl -i -u 'user@example.com:password' \
  -H 'Content-Type: application/vnd.ms-sync.wbxml' \
  --data-binary @/tmp/foldersync-0.wbxml \
  'http://127.0.0.1:18080/Microsoft-Server-ActiveSync?Cmd=FolderSync&User=user@example.com&DeviceId=testdevice&DeviceType=Test' \
  --output /tmp/foldersync-response.wbxml
```

The response is binary WBXML. Inspect it with:

```bash
xxd /tmp/foldersync-response.wbxml | head
```

Planned compatibility harness:

```text
same EAS request -> Z-Push + PR187
                 -> new gateway
                 -> compare decoded WBXML/status/side effects
```

Binary WBXML does not need byte-for-byte equality when semantic decoded output is equivalent.

## Protocol Tests (mock Stalwart)

`src/push_tests.rs` runs the real router over HTTP against a mock Stalwart
(session, JMAP API, EventSource). `cargo test` runs them; no network access.

## End-to-End Tests (real Stalwart)

The `push_tests::e2e` module is `#[ignore]`d and drives a running gateway in
front of a disposable Stalwart with two throwaway accounts. Never point it at
production accounts: it creates and moves mail.

1. Start a test Stalwart (same version as production) with two accounts that
   share a password, and raise its HTTP rate limits for the test
   (`rateLimitAuthenticated` / `rateLimitAnonymous`; Stalwart applies the new
   values after a restart). Seeding uses `Email/set` with inline bodies
   because Stalwart caps temporary blob uploads at 1000 per account.
2. Start the gateway against it with SQLite state, `PING_MIN_SECONDS=1` and
   `PING_RESEND_GUARD_SECONDS=2`.
3. Run:

```bash
E2E_GATEWAY=http://127.0.0.1:18099/Microsoft-Server-ActiveSync \
E2E_JMAP=http://127.0.0.1:18080 \
E2E_USER=alice@e2e.test E2E_USER2=bob@e2e.test E2E_PASS=... \
E2E_N=5000 E2E_LONG_PING=120 E2E_RESTART_CMD=./restart-gateway.sh \
cargo test e2e_ -- --ignored --test-threads=1 --nocapture
```

`E2E_RESTART_CMD` restarts the gateway process (keeping its SQLite file) and
returns once `/healthz` answers.

| Test | Covers |
|---|---|
| `e2e_empty_mailbox` | priming and Sync of an empty folder |
| `e2e_initial_sync_delivers_every_message_exactly_once` | `E2E_N` messages, MoreAvailable paging, no duplicates |
| `e2e_new_mail_during_ping_then_read_move_and_two_devices` | push to two devices at once, read change, move out (Delete) |
| `e2e_lost_response_is_replayed` | resent SyncKey gets the identical reply |
| `e2e_folder_create_rename_delete` | incremental FolderSync |
| `e2e_attachment_download` | FileReference + GetAttachment bytes |
| `e2e_gateway_restart_keeps_device_state` | no re-download, no false Status 2, push resumes |
| `e2e_long_ping_is_held_for_the_heartbeat` | quiet Ping answered Status 1 at the heartbeat, not before |
| `e2e_random_changes_every_status2_is_delivered_and_none_missed` | random new mail, read flips, moves, destroys (server side) and read flips, deletes, MoveItems (device side) on two devices, messages of any age: every Status 2 is deliverable, every change is announced, the acting device is not echoed, both devices end up identical (`E2E_STEPS`, `E2E_SEED`) |

Last full run (2026-11-12, 1:30 am ET): all 9 pass, random test at 100 steps; at `E2E_N=5000` (5000 messages in 50
rounds, ~4 s; new mail -> both Pings answered in ~0.28 s; 120 s Ping held
120.0 s). These are protocol results, not real-device verification.
