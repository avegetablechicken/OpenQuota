# TLS regression fixture

`localhost-key.pem` is a public, test-only RSA PKCS#8 key. Never use it in production.
The TLS tests use rcgen to issue a self-signed CA:TRUE server certificate for
`localhost` at runtime. Its validity is limited to three days around the test run,
so it satisfies macOS lifetime constraints and does not expire in future CI runs.

It reproduces private servers that present their explicitly trusted CA certificate
as the server certificate. Tests check acceptance only with explicit trust, rejection
without trust, with an expired certificate, and when the requested host is absent
from the SAN.

Regenerate using:

```sh
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out localhost-key.pem
```
