# Contacts Mapping

ActiveSync Contacts map to JMAP Contacts/JSContact:

- address books: `AddressBook`
- contacts: `ContactCard`
- names: JSContact `name`
- emails: JSContact `emails`
- phones: JSContact `phones`
- addresses: JSContact `addresses`
- organizations/job titles: JSContact organization fields
- notes: JSContact `notes`
- categories: JSContact `categories`

PR #187 has a useful field-level converter, but implementation should be independent and tested bidirectionally.

