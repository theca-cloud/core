# check=skip=InvalidDefaultArgInFrom
# CI image for the Rust repos: toolchain and tools pre-installed, so pipelines
# don't install them on every run. Built and pushed by scripts/ci-image.sh.
# RUST_VERSION comes from rust-toolchain.toml (exact version, so rustup doesn't
# download another toolchain at runtime).
ARG RUST_VERSION
FROM rust:${RUST_VERSION}
ARG TARGETARCH
ARG GARAGE_VERSION=v2.4.1

RUN rustup component add rustfmt clippy \
 && rustup target add wasm32-unknown-unknown \
 && cargo install --locked cargo-deny \
 && cargo install --locked sqlx-cli --no-default-features --features postgres,rustls \
 && rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git"

# litmus, the WebDAV conformance suite, run by tests/dav.rs.
RUN apt-get update \
 && apt-get install -y --no-install-recommends litmus \
 && rm -rf /var/lib/apt/lists/*

# Garage (S3) for the S3 tests, started by scripts/ci.sh.
RUN case "$TARGETARCH" in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; *) exit 1 ;; esac \
 && curl -fsSL -o /usr/local/bin/garage \
      "https://garagehq.deuxfleurs.fr/_releases/${GARAGE_VERSION}/${arch}-unknown-linux-musl/garage" \
 && chmod +x /usr/local/bin/garage
