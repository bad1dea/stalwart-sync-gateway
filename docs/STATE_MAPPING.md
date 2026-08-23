# State Mapping

ActiveSync:

`SyncKey -> gateway sync record -> JMAP state/queryState`

JMAP:

- Collection hierarchy: `/changes` for `Mailbox`, `AddressBook`, `Calendar`.
- Mail lists: `Email/queryChanges` where query semantics matter.
- Object changes: `Email/changes`, `ContactCard/changes`, `CalendarEvent/changes`.

Gateway state must be schema-versioned and migratable. Do not serialize opaque Rust objects as the persistence format.

