# State Model

ActiveSync SyncKeys are gateway-issued versioned records, not raw JMAP state tokens. A sync record contains:

- user/account
- device id/type
- collection id
- ActiveSync SyncKey generation
- JMAP state token
- optional query state token
- filter/window/body preference
- last successful sync time
- protocol version and provision state

M0 has a `StateStore` trait and memory implementation. SQLite and Redis are configured but not implemented yet.

