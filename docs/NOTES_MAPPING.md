# Notes Mapping

ActiveSync Notes are first-class in the canonical model:

- `Note`
- `NoteId`
- `NoteState`
- `NoteStore`

No native Stalwart JMAP Notes capability has been identified yet. Initial candidates:

1. future native JMAP Notes capability
2. JMAP Email-backed notes in a dedicated Notes mailbox
3. JMAP FileNode-backed notes

The ActiveSync frontend must only depend on `NoteStore`.

