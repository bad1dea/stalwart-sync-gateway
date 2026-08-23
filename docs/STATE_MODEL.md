# State Model

ActiveSync SyncKeys are gateway-issued versioned records, not raw JMAP state tokens. A sync record contains:

- user/account
- device id/type
- collection id
- ActiveSync SyncKey generation
- JMAP state token
- optional query state token
- filter/window/body preference
- last successful sync time
- protocol version and provision state

Current implementation:

- `memory`: development-only, volatile state.
- `sqlite`: single-instance default, stored at `STATE_SQLITE_PATH`.
- `redis`: configured placeholder for a later multi-replica backend.

SQLite uses explicit migrations and a `sync_records` table keyed by `(user, device_id, collection_id)`. The current M1 mail receive path persists the ActiveSync SyncKey and a seen JMAP Email id set so repeat `Sync` requests do not resend the same mailbox contents. The next state upgrade is to persist JMAP `queryState`/object state and drive mail changes from `Email/queryChanges` and `Email/changes`.
