Test-only CA and client/server identity for loopback TLS integration tests.
The private key is deliberately public and must never be used outside tests.
The certificate covers localhost and 127.0.0.1 and expires in 2126.
The CA signing key is not retained. No external certificate tools are needed
when running the tests.
