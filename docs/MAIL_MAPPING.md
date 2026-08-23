# Mail Mapping

ActiveSync Mail maps to JMAP Mail:

- folders: `Mailbox`
- messages: `Email`
- conversations: `Thread`
- attachments: `Blob`
- send: `EmailSubmission`
- flags: JMAP keywords such as `$seen`, `$flagged`, `$answered`, `$forwarded`, `$draft`

M1 will implement receive/sync, read/unread, move, delete. M2 will implement send/reply/forward and attachments.

