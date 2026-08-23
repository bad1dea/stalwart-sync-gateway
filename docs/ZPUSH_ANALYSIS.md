# Z-Push Analysis

Z-Push is a PHP ActiveSync server framework. Its useful behavioral references are the HTTP and ActiveSync command surfaces, WBXML tag tables, SyncKey state machine, device tracking, and real-client compatibility decisions.

Key files:

- `src/index.php`: entry point, auth, OPTIONS, dispatch, response headers.
- `src/lib/request/request.php`: query/header/auth parsing, compact base64 query handling.
- `src/lib/request/requestprocessor.php`: command handler dispatch and WBXML wiring.
- `src/lib/request/sync.php`, `foldersync.php`, `ping.php`, `provisioning.php`: core sync behavior.
- `src/lib/core/synccollections.php`: Sync and Ping collection state.
- `src/lib/core/devicemanager.php`: device state and hierarchy tracking.
- `src/lib/wbxml/*`: ActiveSync WBXML.

Reuse strategy: use Z-Push as an oracle and compatibility guide. Do not translate source line-by-line.

