# Push Architecture

Target design:

1. ActiveSync client sends `Ping` with folders and heartbeat.
2. Gateway registers a pending waiter keyed by user, device, and collections.
3. A per-user JMAP EventSource connection receives Stalwart state-change notifications.
4. Gateway maps changed JMAP data types to ActiveSync collections.
5. Matching Ping waiters wake and return changed folders.
6. Client follows with `Sync`; gateway uses `/changes` or `queryChanges`.

Stalwart docs state EventSource is listed in the JMAP Session resource and available at `/jmap/eventsource/?types={types}&closeafter={closeafter}&ping={ping}`. This should replace Z-Push PR #187's five-second polling loop.

Current implementation status:

- `Ping` has a bounded long-poll compatibility response.
- It returns heartbeat-expired status when no JMAP wakeup layer is available.
- The next implementation step is a per-account JMAP EventSource/WebSocket subscriber that wakes matching pending Ping requests and returns changed collection ids.
