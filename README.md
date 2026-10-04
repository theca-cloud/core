# Theca core

The Theca server and the Rust libraries shared by its clients:

- `common`: API types, chunking, hashing, name rules (native + WASM).
- `core`: the server (API, WebDAV, background jobs, migrations).
- `client`: the engine every Rust client is built on (auth, upload, download).

Design and decisions: [theca/docs](https://git.pband.ch/theca/docs). Contributing: see [CONTRIBUTING.md](https://git.pband.ch/theca/docs/src/branch/prod/CONTRIBUTING.md).

License: AGPL-3.0.
