# Changelog

Every release of `core`: the server image, the crates, the browser package, `openapi.json`.
## [0.17.0] - 2026-10-08

### Features

- **core**: Secret files are only read, an empty key file stops the server (**breaking**)
## [0.16.0] - 2026-10-08

### Features

- **core**: The admin gives an organization another owner
## [0.15.0] - 2026-10-08

### Features

- **core**: Me says whether the account may create organizations
## [0.14.1] - 2026-10-08

### Fixes

- **core**: Every API operation has its own id
## [0.14.0] - 2026-10-08

### Features

- **core**: Password strength (zxcvbn), password change, a link's email (**breaking**)
## [0.13.0] - 2026-10-08

### Features

- **core**: Search in an organization or below a folder
## [0.12.0] - 2026-10-08

### Features

- **core**: A file always has a current version, checked at commit (**breaking**)
## [0.11.1] - 2026-10-08

### Fixes

- **core**: Query parameters are declared as such in openapi.json
## [0.11.0] - 2026-10-08

### Features

- **core**: Node operations take lists: PATCH /nodes, POST /nodes/copy, /nodes/trash, /nodes/restore (**breaking**)

### Fixes

- **core**: Logout needs no valid session, and always clears the cookie
## [0.10.0] - 2026-10-07

### Features

- **core**: GET /instance, the instance's name and whether accounts can be created, for login pages
## [0.9.0] - 2026-10-07

### Features

- What the web app needs (members, namespaces, organizations, shared client code, the WASM API) (**breaking**)
## [0.8.0] - 2026-10-07

### Features

- **core**: One-way doors reviewed (step 15): object layout, RFC 3339 dates (**breaking**)

### Fixes

- **ci**: Skip pushes that change nothing built, and the release merge back into dev
- **ci**: CONFIGURATION.md triggers the checks (a test reads it)
## [0.7.0] - 2026-10-07

### Features

- **core**: GET /nodes/{id}/tree streamed; folders created from a list of paths (**breaking**)
- **core**: File sizes, summed from their chunks at commit
## [0.5.0] - 2026-10-07

### Features

- **client**: A staged upload engine for many files; feat(core): batch commits
- **core**: Commits only in batches; feat(client): cancelling one file of the upload engine (**breaking**)
- **core**: Lists everywhere (POST /files/commit, POST /files/download); feat(client): a staged download engine (**breaking**)
- **core**: GET /limits; feat(client): the upload window follows the server, 16 transfers at once

### Fixes

- **ci**: Release notes keep their line breaks (curl -d strips them)
- **ci**: The release JSON sent as is (sh's echo turned its \n into line breaks: 422)
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
