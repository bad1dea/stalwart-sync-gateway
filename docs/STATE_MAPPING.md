# State Mapping

ActiveSync:

`SyncKey -> gateway sync record -> JMAP state/queryState`

JMAP:

- Collection hierarchy: `/changes` for `Mailbox`, `AddressBook`, `Calendar`.
- Mail lists: `Email/queryChanges` where query semantics matter.
- Object changes: `Email/changes`, `ContactCard/changes`, `CalendarEvent/changes`.

Gateway state must be schema-versioned and migratable. Do not serialize opaque Rust objects as the persistence format.

Implemented M1 mapping:

- Initial mail `Sync` with `SyncKey=0` returns the current JMAP Email page for the mailbox and persists the seen Email ids.
- Later mail `Sync` requests must present the last gateway-issued SyncKey for that device and collection.
- If no unseen Email ids are returned by the current query page, the gateway returns success without advancing the SyncKey.
- If new Email ids are observed, the gateway returns `Add` commands, advances the SyncKey, and updates the persisted seen-id set.

This is intentionally conservative. It prevents duplicate Adds for device testing, but it is not the final change engine. The target production mapping is:

`ActiveSync SyncKey -> SQLite/Redis record -> JMAP queryState/object state -> JMAP /queryChanges or /changes`
