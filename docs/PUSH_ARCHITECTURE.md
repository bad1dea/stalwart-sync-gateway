# Push Architecture

Target design:

1. ActiveSync client sends `Ping` with folders and heartbeat.
2. Gateway registers a pending waiter keyed by user, device, and collections.
3. A per-user JMAP EventSource connection receives Stalwart state-change notifications.
4. Gateway maps changed JMAP data types to ActiveSync collections.
5. Matching Ping waiters wake and return changed folders.
6. Client follows with `Sync`; gateway uses `/changes` or `queryChanges`.

Stalwart docs state EventSource is listed in the JMAP Session resource and available at `/jmap/eventsource/?types={types}&closeafter={closeafter}&ping={ping}`. This should replace Z-Push PR #187's five-second polling loop.

Implemented (src/push.rs, Ping in src/activesync.rs):

- One JMAP EventSource connection per account (session `eventSourceUrl`, `types=*`), shared by all devices; reconnect with 2-60s backoff, stops on 401/403 or after `PUSH_IDLE_SECONDS` without Ping activity.
- Events are wake-ups only. Ping re-checks the mailbox against the device's stored seen-ids (`mailbox_has_sync_changes`) and answers Status 2 only when Sync would send something (new, moved-in, deleted, moved-out).
- Lost-event safety: the check runs at Ping start, the waiter is registered before the check, and every (re)connect forces a re-check.
- Status 1 on heartbeat expiry, 3 missing parameters, 5 heartbeat outside `PING_MIN_SECONDS`..`PING_MAX_SECONDS` (limit returned), 7 unknown folder. Empty-body Ping reuses the device's cached parameters.
- A newer Ping from the same device supersedes the older one. `PING_MAX_CONCURRENT` caps pending Pings (503 + Retry-After).

Not push-woken: Notes, Contacts, Calendar, Tasks (they sync on the client's schedule); server-side read-flag changes (Sync cannot deliver them).
