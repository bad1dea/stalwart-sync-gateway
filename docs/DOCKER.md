# Docker

The production image is a multi-stage Rust build. Runtime contains the gateway binary and CA certificates, runs as UID `10001`, exposes `8080`, and writes persistent state under `/data`.

Example:

```bash
docker run -d \
  --name stalwart-sync-gateway \
  --network mail \
  -p 8080:8080 \
  -e STALWART_JMAP_URL=http://stalwart:8080/.well-known/jmap \
  -e EAS_PUBLIC_URL=https://mail.example.com/Microsoft-Server-ActiveSync \
  -e STATE_BACKEND=sqlite \
  -e STATE_SQLITE_PATH=/data/state.db \
  -v stalwart-sync-data:/data \
  stalwart-sync-gateway:latest
```

