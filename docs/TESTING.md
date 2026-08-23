# Testing

M0 unit tests cover WBXML decode and encode/decode round trips.

## Manual ActiveSync Smoke Test

With the gateway running on `127.0.0.1:18080`, create an initial FolderSync WBXML request:

```bash
printf '\x03\x01\x6a\x00\x00\x07\x56\x52\x03\x30\x00\x01\x01' > /tmp/foldersync-0.wbxml
```

Then call the gateway with a real Stalwart account:

```bash
curl -i -u 'user@example.com:password' \
  -H 'Content-Type: application/vnd.ms-sync.wbxml' \
  --data-binary @/tmp/foldersync-0.wbxml \
  'http://127.0.0.1:18080/Microsoft-Server-ActiveSync?Cmd=FolderSync&User=user@example.com&DeviceId=testdevice&DeviceType=Test' \
  --output /tmp/foldersync-response.wbxml
```

The response is binary WBXML. Inspect it with:

```bash
xxd /tmp/foldersync-response.wbxml | head
```

Planned compatibility harness:

```text
same EAS request -> Z-Push + PR187
                 -> new gateway
                 -> compare decoded WBXML/status/side effects
```

Binary WBXML does not need byte-for-byte equality when semantic decoded output is equivalent.
