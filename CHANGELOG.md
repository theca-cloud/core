# Changelog

Every release of `core`: the server image, the crates, the browser package, `openapi.json`.
## [0.5.0] - 2026-10-06

### Features

- **client**: A staged upload engine for many files; feat(core): batch commits
- **core**: Commits only in batches; feat(client): cancelling one file of the upload engine (**breaking**)
- **core**: Lists everywhere (POST /files/commit, POST /files/download); feat(client): a staged download engine (**breaking**)
- **core**: GET /limits; feat(client): the upload window follows the server, 16 transfers at once

### Fixes

- **ci**: Release notes keep their line breaks (curl -d strips them)
## [0.4.0] - 2026-10-06

### Features

- **core**: Library trash and rename, moves and copies onto files, RCS version numbers
## [0.3.0] - 2026-10-06

### Features

- **core**: Unchanged content creates no version; listings carry the current version
## [0.2.0] - 2026-10-06

### Features

- **client**: Logout

### Fixes

- **ci**: The release commit updates openapi.json, which carries the version

### Build and CI

- Try/, a local instance from the released image
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

### Fixes

- **ci**: Run the release pipeline on manual triggers too
- **ci**: Send the registry token, re-runnable publish
- **client**: Allow the wasm features Rust emits in wasm-opt

### Build and CI

- Split the pipeline into steps, skip documentation-only changes
- Release pipeline, images and packages
- Dry-run the publish step locally
