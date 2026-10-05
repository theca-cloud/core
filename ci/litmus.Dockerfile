# litmus, the WebDAV (RFC 4918) conformance suite, for running the dav tests
# locally (tests/dav.rs builds and runs it when litmus isn't installed). The
# CI image installs the same Debian package.
FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends litmus \
 && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["litmus"]
