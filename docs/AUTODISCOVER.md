# Autodiscover

Endpoint:

`POST /Autodiscover/Autodiscover.xml`

The response advertises a `MobileSync` server URL from `EAS_PUBLIC_URL`, for example:

`https://mail.example.com/Microsoft-Server-ActiveSync`

Production reverse proxies should route both `https://autodiscover.example.com/Autodiscover/Autodiscover.xml` and `https://example.com/Autodiscover/Autodiscover.xml` to the gateway.

