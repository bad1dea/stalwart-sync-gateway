# PR #187 Analysis

PR #187 adds a JMAP backend to Z-Push. It maps Z-Push `BackendDiff` calls to Stalwart JMAP.

Strengths:

- Uses JMAP Session discovery.
- Uses JMAP method batching for query+get.
- Covers mail, contacts, calendars, attachment download, and mail submission.
- Provides concrete JSContact and JSCalendar mapping examples.

Weaknesses for this project:

- It is still shaped by Z-Push backend interfaces.
- Mail folders keep raw JMAP IDs while contacts/calendars get synthetic prefixes.
- Push uses periodic polling of query state.
- It does not define a protocol-independent canonical model.
- Notes and tasks are not first-class enough for this project.

The new gateway will reproduce required wire behavior, not PHP architecture.

