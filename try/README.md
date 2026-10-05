# Try Theca locally

The released server image with Postgres, set up and ready for a WebDAV client: a quick way to use Theca as a user would. Not for real data, nor a production setup (no TLS, a fixed admin password).

Needs Docker, `curl`, `jq`, `openssl`, and `docker login git.pband.ch` (the image is private).

```sh
./run.sh          # start; the first run sets up an admin, a "Demo" organization,
                  # a "Documents" library and an app password, then prints them
./run.sh --down   # stop, keeping the data
./run.sh --clean  # stop and delete everything (database, files, secrets)
```

The credentials are kept in `data/credentials.txt`. Another image: `THECA_IMAGE=git.pband.ch/theca/core:rc ./run.sh` (the release candidate), or a local build.

## WebDAV

`http://localhost:8090/dav/`, any username, the app password as the password. The drive shows organizations, then their libraries: files go in `Demo/Documents/`.

- **Finder**: Go → Connect to Server (⌘K), `http://localhost:8090/dav/`.
- **Cyberduck**: Open Connection → WebDAV (HTTP), server `localhost`, port `8090`, path `/dav/`.
- **rclone** (`brew install rclone`), scriptable:
  ```sh
  rclone config create theca webdav url=http://localhost:8090/dav/ vendor=other user=me pass="$(rclone obscure '<app password>')"
  rclone copy ~/Pictures theca:Demo/Documents/Pictures --progress
  rclone check ~/Pictures theca:Demo/Documents/Pictures
  ```
