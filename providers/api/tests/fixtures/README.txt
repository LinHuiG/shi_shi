untrusted-localhost.pfx is a synthetic, self-signed localhost certificate.
Password: synthetic-test-only. Its private key is deliberately public test data.
Generated with .NET CertificateRequest, RSA-2048, SHA-256, SAN localhost/127.0.0.1.
Validity: 2020-01-01 through 2100-01-01. Never install it into a trust store.
The HTTP regression checks that the real TLS backend rejects this certificate
and exposes only the safe TLS error class, before sending an HTTP request.
