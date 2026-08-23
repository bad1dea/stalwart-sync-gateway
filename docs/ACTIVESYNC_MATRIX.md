# ActiveSync Matrix

| Command | MVP | Mapping | Status |
| --- | --- | --- | --- |
| OPTIONS | M0 | protocol/version advertisement | implemented |
| FolderSync | M1 | JMAP `Mailbox/get`, `AddressBook/get`, `Calendar/get` | initial full hierarchy implemented |
| Sync | M1 | JMAP `Email/query`, `Email/get`, `Email/set` | initial mail receive, read/unread, delete implemented |
| Ping | M3 | JMAP EventSource wakes waiters | bounded heartbeat-expired response implemented; JMAP wakeups planned |
| SendMail | M2 | Blob upload, Email import/create, EmailSubmission/set | planned |
| SmartReply | M2 | MIME construction + submission + answered keyword | planned |
| SmartForward | M2 | MIME construction + submission + forwarded keyword | planned |
| GetItemEstimate | M1 | conservative per-folder estimate | compatibility response implemented |
| ItemOperations | M1/M2 | fetch items/attachments | planned |
| MoveItems | M1 | `Email/set` mailboxIds | implemented |
| MeetingResponse | M5 | CalendarEvent participant status/scheduling | planned |
| Search | M8 | JMAP query/search where supported | planned |
| Settings | M8 | device/account settings subset | minimal success + user information implemented |
| ResolveRecipients | M8 | contacts/mail address lookup | planned |
| Provision | M8 | minimal policy compatibility | no-policy response implemented |
| ValidateCert | later | likely no-op/status compatibility | planned |
| Find | M8 | EAS 16 search-like command | planned |
| FolderCreate/Delete/Update | M1+ | Mailbox/AddressBook/Calendar set | planned |
