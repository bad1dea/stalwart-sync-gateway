# Calendar Mapping

ActiveSync Calendar maps to JMAP Calendars/JSCalendar:

- calendars: `Calendar`
- events: `CalendarEvent`
- title/start/duration/time zone map directly to JSCalendar event fields.
- attendees and organizers map to JSCalendar `participants`.
- reminders map to JSCalendar `alerts`.
- recurrence maps to JSCalendar `recurrenceRules` and exceptions.

Meeting requests/responses require additional scheduling behavior and real-client testing.

