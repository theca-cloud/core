# Configuration

How to configure a Theca server (`theca-server`). Design and reasons: [decisions](https://git.pband.ch/theca/docs) (Configuration).

Two kinds of configuration:

- **Environment variables**: only what the server needs to start and reach its database. Read once at startup.
- **Instance settings**: everything else, stored in the database (`setting` table), editable from the admin interface without a restart.

A test (`tests/configuration.rs`) fails if a variable or a setting read by the code is missing from this file.

## Environment variables

**Secrets only exist as `X__FILE`**: the variable holds the path to a file containing the value (a trailing newline is ignored). Everything else is a plain variable.

| Variable | Required | Default | Description |
|---|---|---|---|
| `DATABASE_URL__FILE` | yes | | File containing the PostgreSQL URL, e.g. `postgres://user:password@host:5432/theca`. PostgreSQL 18+ with the built-in `C.UTF-8` collation (`initdb --locale-provider=builtin --builtin-locale=C.UTF-8`); the server refuses to start otherwise. |
| `JWT_KEY__FILE` | yes | | File containing the server's key (64 hex characters). If the file is empty, the server generates a key and writes it there (the file must then be writable). Also derives the key signing the server's own upload/download URLs. |
| `PUBLIC_URL` | yes | | The URL clients reach the server at, e.g. `https://cloud.example.com`. Used in links and in the server's own presigned URLs. |
| `LISTEN_ADDR` | no | `0.0.0.0:8080` | Address and port the server listens on. |
| `RUST_LOG` | no | `info` | Log filter (`tracing` syntax), e.g. `info,theca_core=debug`. |

## Instance settings

Stored as strings in the `setting` table and typed by the server. A missing key takes its default; an invalid value stops the server at startup; an unknown key is logged as a warning.

| Key | Default | Description |
|---|---|---|
| `instance_name` | `Theca` | Shown in emails and the web app. |
| `gc_grace_period_secs` | `86400` (24 h) | How long an unreferenced chunk, untouched, is kept before garbage collection deletes it (abandoned uploads). |
| `presigned_url_lifetime_secs` | `900` (15 min) | How long presigned upload and download URLs stay valid. |
| `max_pending_uploads` | `15` | Most chunks a user may have being uploaded at once (declared, upload URL still valid, not uploaded yet). Requesting more upload URLs is refused (`429`); clients upload in batches. |

## Storage backends

Storage backends are rows of the `storage` table (created by the setup wizard). Their `details` depend on the implementation. Credentials are **file paths**, never values.

| Implementation | `details` |
|---|---|
| `filesystem` | `{"path": "/data/chunks"}`: a directory the server can write to. Uploads and downloads go through the server (`/api/v1/blobs`, URLs signed by the server). |
| `s3` | `{"host": "https://s3.example.com", "region": "...", "bucket": "...", "access_key_file": "/run/secrets/s3_access", "secret_key_file": "/run/secrets/s3_secret", "path_style": true}`. Clients upload and download directly with S3 presigned URLs. `path_style` (default `true`): `host/bucket/key` URLs, what self-hosted S3 servers (e.g. Garage) usually need. |

## Health check

`theca-server healthcheck` probes `/api/health` on `LISTEN_ADDR` and exits `0` (ready) or `1`: use it as Docker's `HEALTHCHECK`. The server only listens once its database is migrated, so give it a start period.
