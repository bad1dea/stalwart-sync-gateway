# ActiveSync Matrix

| Command | MVP | Mapping | Status |
| --- | --- | --- | --- |
| OPTIONS | M0 | protocol/version advertisement | scaffolded |
| FolderSync | M1 | JMAP `Mailbox/get`, `AddressBook/get`, `Calendar/get` | initial full hierarchy implemented |
| Sync | M1 | JMAP query/get/changes/queryChanges | planned |
| Ping | M3 | JMAP EventSource wakes waiters | planned |
| SendMail | M2 | Blob upload, Email import/create, EmailSubmission/set | planned |
| SmartReply | M2 | MIME construction + submission + answered keyword | planned |
| SmartForward | M2 | MIME construction + submission + forwarded keyword | planned |
| GetItemEstimate | M1 | query count/changes estimate | planned |
| ItemOperations | M1/M2 | fetch items/attachments | planned |
| MoveItems | M1 | Email/set mailboxIds | planned |
| MeetingResponse | M5 | CalendarEvent participant status/scheduling | planned |
| Search | M8 | JMAP query/search where supported | planned |
| Settings | M8 | device/account settings subset | planned |
| ResolveRecipients | M8 | contacts/mail address lookup | planned |
| Provision | M8 | minimal policy compatibility | planned |
| ValidateCert | later | likely no-op/status compatibility | planned |
| Find | M8 | EAS 16 search-like command | planned |
| FolderCreate/Delete/Update | M1+ | Mailbox/AddressBook/Calendar set | planned |
