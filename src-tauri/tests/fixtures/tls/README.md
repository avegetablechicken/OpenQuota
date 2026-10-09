# TLS regression fixture

`localhost-ca.pem` is a self-signed CA:TRUE server certificate for `localhost`,
valid for ten years from October 2026. Its accompanying PKCS#8 key is public test
data only. Never install this fixture in a system trust store or use it in production.

It reproduces private servers that present their explicitly trusted CA certificate
as the server certificate. Tests check acceptance only with explicit trust, rejection
without trust, and rejection when the requested host is absent from the SAN.

Regenerate using:

```sh
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout localhost-key.pem -out localhost-ca.pem -days 3650 \
  -subj '/CN=OpenQuota TLS test only' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -addext 'keyUsage=critical,digitalSignature,keyEncipherment,keyCertSign' \
  -addext 'extendedKeyUsage=serverAuth' \
  -addext 'subjectAltName=DNS:localhost'
```
