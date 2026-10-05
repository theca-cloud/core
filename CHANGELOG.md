# Changelog

Every release of `core`: the server image, the crates, the browser package, `openapi.json`.
## [0.1.0] - 2026-10-05

### Features

- **core**: Add workspace skeleton, dev environment and CI
- **common**: Add chunking, hashing, compression and name rules
- **core**: Add server foundation
- **core**: Full schema and vertical slice of the upload protocol
- **core**: S3 backend, signed chunk declarations, quota and abuse protections
- **core**: Identity and authentication (**breaking**)
- **core**: First-run setup
- **core**: File operations
- **core**: Background jobs
- **core**: Emails, registration, invitations and members
- **core**: WebDAV and streaming previews
- **client**: The client engine, with an end-to-end smoke test
- **core**: Admin API and live settings
- **core**: Rate limits, trusted proxies and metrics
- **core**: Account and organization deletion, export

### Build and CI

- Split the pipeline into steps, skip documentation-only changes
- Release pipeline, images and packages
