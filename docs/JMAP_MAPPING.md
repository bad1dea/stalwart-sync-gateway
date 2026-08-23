# JMAP Mapping

Initial method mapping:

| Canonical operation | JMAP method candidates |
| --- | --- |
| discover mailboxes | `Mailbox/get`, `Mailbox/changes` |
| query mail in mailbox | `Email/query`, `Email/queryChanges` |
| fetch mail | `Email/get`, `Blob/download` |
| update flags | `Email/set` keyword patches |
| move mail | `Email/set` mailboxIds patch |
| delete mail | `Email/set destroy` or move to trash |
| upload/send | `Blob/upload`, `Email/import`, `EmailSubmission/set` |
| discover address books | `AddressBook/get`, `AddressBook/changes` |
| sync contacts | `ContactCard/query/get/set/changes/queryChanges` |
| discover calendars | `Calendar/get`, `Calendar/changes` |
| sync events/tasks | `CalendarEvent/query/get/set/changes/queryChanges` |
| attachments | `Blob/download`, `Blob/upload` |
| push | Session `eventSourceUrl`, optional push subscriptions |

PR #187 confirms these names against Stalwart-era JMAP but the gateway will continue to trust current Session capabilities and Stalwart behavior over the PR.

