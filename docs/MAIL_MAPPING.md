# Mail Mapping

ActiveSync Mail maps to JMAP Mail:

- folders: `Mailbox`
- messages: `Email`
- conversations: `Thread`
- attachments: `Blob`
- send: `EmailSubmission`
- flags: JMAP keywords such as `$seen`, `$flagged`, `$answered`, `$forwarded`, `$draft`

Implemented M1 coverage:

- `FolderSync`: JMAP `Mailbox/get` plus first-class contact/calendar collection discovery.
- Mail receive `Sync`: JMAP `Email/query` batched with `Email/get`.
- Repeat mail `Sync`: gateway state suppresses duplicate `Add` replay for already-seen Email ids.
- Read/unread from client `Sync Change`: JMAP `Email/set` patch to `keywords/$seen`.
- Delete from client `Sync Delete`: JMAP `Email/set destroy`.

Remaining M1 mail work:

- move between folders
- incremental JMAP `Email/queryChanges`/`Email/changes`
- server-side deletions returned as ActiveSync deletes
- richer body preference handling

M2 will implement send/reply/forward and attachments.
