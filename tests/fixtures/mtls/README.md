# Public test-only TLS credentials

These PEM files are deliberately public fixtures for the local mTLS integration
test. Never use these keys outside tests. The test CA signs separate server and
client certificates; the server certificate covers 127.0.0.1 and localhost.
Certificates expire ten years after generation in 2026. The CA private key was
discarded after generation. No external service is used by the tests.
