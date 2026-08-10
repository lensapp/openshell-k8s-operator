# syntax=docker/dockerfile:1

# Build both workspace binaries. The builder matches the crates' rust-version;
# the git dependencies on NVIDIA/OpenShell are fetched at build time (pinned by
# the committed Cargo.lock, so the build is reproducible).
FROM rust:1.90-bookworm AS builder
WORKDIR /build
COPY . .
# Cache the cargo registry and target dir across builds (BuildKit). The target
# dir is a cache mount, so copy the finished binaries out to real layer paths.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin openshell-operator --bin openshell-issuer \
    && cp target/release/openshell-operator /openshell-operator \
    && cp target/release/openshell-issuer /openshell-issuer

# The static OIDC issuer (mint + serve). Build with `--target issuer`.
FROM gcr.io/distroless/cc-debian12:nonroot AS issuer
COPY --from=builder /openshell-issuer /usr/local/bin/openshell-issuer
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/openshell-issuer"]

# The operator. Last stage, so a plain `docker build .` yields the operator
# image. Distroless cc: glibc + CA certificates, no shell or package manager,
# runs as the built-in nonroot user (uid 65532). The CA bundle is load-bearing:
# outbound TLS resolves roots through rustls-native-certs (tonic for the
# gateway channel, reqwest for HTTP), so a base without the bundle at
# /etc/ssl/certs/ca-certificates.crt breaks HTTPS.
FROM gcr.io/distroless/cc-debian12:nonroot AS operator
COPY --from=builder /openshell-operator /usr/local/bin/openshell-operator
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/openshell-operator"]
