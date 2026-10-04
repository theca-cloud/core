# check=skip=InvalidDefaultArgInFrom
# CI image for the Rust repos: toolchain and tools pre-installed, so pipelines
# don't install them on every run. Built and pushed by scripts/ci-image.sh.
# RUST_VERSION comes from rust-toolchain.toml (exact version, so rustup doesn't
# download another toolchain at runtime).
ARG RUST_VERSION
FROM rust:${RUST_VERSION}

RUN rustup component add rustfmt clippy \
 && rustup target add wasm32-unknown-unknown \
 && cargo install --locked cargo-deny \
 && rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git"
