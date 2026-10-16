# The core image: the web app's dev and test server only, never shipped (users
# run the CE image, which compiles the server itself). Nothing is compiled
# here: the pipeline cross-compiles static binaries (scripts/release-ci.sh
# binaries) into dist/<arch>/, and each platform's image copies its own.
FROM gcr.io/distroless/static-debian12:nonroot
ARG TARGETARCH
COPY dist/${TARGETARCH}/theca-server /theca-server
# Plain HTTP: TLS is the reverse proxy's (declare it in TRUSTED_PROXIES).
ENV LISTEN_ADDR=0.0.0.0:8080
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s CMD ["/theca-server", "healthcheck"]
ENTRYPOINT ["/theca-server"]
