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
| `SERVER_KEY__FILE` | yes | | File containing the server's key (64 hex characters). If the file is empty, the server generates a key and writes it there (the file must then be writable). Derives the key signing the server's own upload/download URLs: rotating it only invalidates the URLs in flight. |
| `DATA_KEY__FILE` | yes | | File containing the key encrypting secrets stored in the database (TOTP secrets), 64 hex characters; generated into the file if empty. **Back it up with the database and never change it**: without it, those secrets are lost (users must set up TOTP again). |
| `SETUP_OPEN` | no | `false` | `true`: a fresh instance's setup needs no token (the first visitor sets it up). Otherwise the server prints the setup URL, token included, in its logs at startup. |
| `TRUSTED_PROXIES` | no | | Comma-separated addresses or CIDRs of the reverse proxies in front of the server (e.g. `172.17.0.1, 10.0.0.0/8`). Their `X-Forwarded-For` gives the client's address. Unset: the peer is the client, so behind an undeclared proxy every client shares the proxy's address (and its rate limits). |
| `METRICS_ADDR` | no | | Where Prometheus metrics are served (`/metrics`), e.g. `127.0.0.1:9090` or an internal address. Unset: no metrics. Never the public port. |
| `WEB_DIR` | no | | A directory of static files served for every path the API doesn't use (the web app's build; `index.html` for unknown paths, as a single-page app needs). Unknown `/api/` and `/dav/` paths stay `404`. Set by the CE image. |
| `PUBLIC_URL` | yes | | The URL clients reach the server at, e.g. `https://cloud.example.com`. Used in links and in the server's own presigned URLs. |
| `LISTEN_ADDR` | no | `0.0.0.0:8080` | Address and port the server listens on. |
| `RUST_LOG` | no | `info` | Log filter (`tracing` syntax), e.g. `info,theca_core=debug`. |

## Instance settings

Changed through the admin API (`PATCH /api/v1/admin/settings`), which validates them and applies them at once, without a restart. Stored as strings in the `setting` table and typed by the server. A missing key takes its default; an invalid value is refused by the API (and stops the server at startup if written to the database by hand); an unknown key is logged as a warning.

| Key | Default | Description |
|---|---|---|
| `instance_name` | `Theca` | Shown in emails and the web app. |
| `gc_grace_period_secs` | `86400` (24 h) | How long an unreferenced chunk, untouched, is kept before garbage collection deletes it (abandoned uploads). |
| `presigned_url_lifetime_secs` | `900` (15 min) | How long presigned upload and download URLs stay valid. |
| `max_pending_uploads` | `15` | Most chunks a user may have being uploaded at once (declared, upload URL still valid, not uploaded yet). Requesting more upload URLs is refused (`429`); clients upload in batches. |
| `registration` | `closed` | `open`: anyone can create an account (verified by email, so it needs SMTP). `closed`: accounts come from invitations (and the setup). |
| `smtp_host` | | The SMTP server. Unset: emails stay queued (and registration and invitations are unavailable). Changes apply to the next sending run, no restart needed. |
| `smtp_port` | `587` | Its port. |
| `smtp_tls` | `starttls` | `starttls` (587), `tls` (465), or `none` (a server on the same host or network only). |
| `smtp_username` | | Login, if the server needs one. |
| `smtp_password_file` | | Path to a file holding the SMTP password (never the password itself). |
| `smtp_from` | | The sender, e.g. `Theca <noreply@example.com>`. Required with `smtp_host`. |
| `rate_limit_login_ip` | `30/300` | Login attempts from one client address: count/seconds. Beyond: `429` with `Retry-After`. Limits slow guessing down; none locks anyone out for good. |
| `rate_limit_login_account` | `10/300` | **Failed** logins on one account (a user logging in normally is never slowed down). |
| `rate_limit_totp` | `10/300` | Failed TOTP or recovery codes on one account. |
| `rate_limit_email_address` | `5/3600` | Emails asked for one address (registration, verification, password reset). |
| `rate_limit_email_ip` | `30/3600` | Emails asked from one client address. |
| `rate_limit_setup_ip` | `10/60` | Setup attempts from one client address (the setup token). |
| `rate_limit_invitations` | `100/3600` | Members added to one organization. |
| `job_chunk_gc_interval_secs` | `3600` (1 h) | How often unreferenced chunks are collected. |
| `job_trash_purge_interval_secs` | `3600` (1 h) | How often the trash past its retention is purged. |
| `job_cleanup_interval_secs` | `86400` (24 h) | How often expired tokens, invitations, unverified accounts, old emails and job history are cleaned. |
| `job_orphan_check_interval_secs` | `86400` (24 h) | How often storage backends are checked for objects without a row. |
| `job_email_send_interval_secs` | `30` | How often queued emails are sent. |
| `trash_retention_secs` | `2592000` (30 days) | How long trashed files and folders stay restorable; then the hourly purge deletes them for good (their chunks follow with the garbage collection). `0`: purged at the next run. |
| `session_idle_lifetime_secs` | `604800` (7 days) | A session expires after this long without being used (each use extends it). At least `600`. Applies to existing sessions at once. |
| `oidc_link_by_email` | `false` | Logging in through an OIDC provider with an identity not linked yet links it to the account with the same email, if the provider says that email is verified. Off: identities are only linked by their owner, from a session. Turn on only if you trust every provider's email verification (an organization's provider still only opens sessions restricted to that organization). |
| `oidc_allow_internal_issuers` | `false` | Organizations' identity providers may use `http` and internal addresses (private networks, localhost). Off, an organization's issuer must be `https` and resolve to public addresses only, so an organization's owner can't make the server call into its own network. Turn on only when every organization on the instance is trusted (e.g. a single company's install with its Keycloak inside). Instance-wide providers are never restricted. |
| `session_max_lifetime_secs` | `0` (no cap) | A session expires this long after login, even if used every day (users log in again). At least `600`, or `0` for no cap. |

## Storage backends

Storage backends are rows of the `storage` table (created by the setup wizard). Their `details` depend on the implementation. Credentials are **file paths**, never values.

| Implementation | `details` |
|---|---|
| `filesystem` | `{"path": "/data/chunks"}`: a directory the server can write to. Uploads and downloads go through the server (`/api/v1/blobs`, URLs signed by the server). |
| `s3` | `{"host": "https://s3.example.com", "region": "...", "bucket": "...", "access_key_file": "/run/secrets/s3_access", "secret_key_file": "/run/secrets/s3_secret", "path_style": true}`. Clients upload and download directly with S3 presigned URLs. `path_style` (default `true`): `host/bucket/key` URLs, what self-hosted S3 servers (e.g. Garage) usually need. |

## WebDAV

Libraries mount as network drives at `{PUBLIC_URL}/dav/`, with an **app password** as the password (any username). The drive follows the app password's scope: the whole account (organizations, then libraries), one organization (its libraries), or one library (its files); a read-only app password gives a read-only drive.

Uploads through WebDAV are spooled to the server's temporary directory (`TMPDIR`, `/tmp` by default) before being chunked and stored: it needs as much free space as the largest files being uploaded at once.

## Health check

`theca-server healthcheck` probes `/api/health` on `LISTEN_ADDR` and exits `0` (ready) or `1`: use it as Docker's `HEALTHCHECK`. The server only listens once its database is migrated, so give it a start period.
