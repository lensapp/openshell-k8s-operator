# syntax=docker/dockerfile:1

# Build the operator binary. The builder matches the crate's rust-version; the
# git dependencies on NVIDIA/OpenShell are fetched at build time (pinned by the
# committed Cargo.lock, so the build is reproducible).
FROM rust:1.90-bookworm AS builder
WORKDIR /build
COPY . .
# Cache the cargo registry and target dir across builds (BuildKit). The target
# dir is a cache mount, so copy the finished binary out to a real layer path.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin openshell-operator \
    && cp target/release/openshell-operator /openshell-operator

# Distroless cc image: glibc + CA certificates, no shell or package manager.
# Runs as the built-in nonroot user (uid 65532).
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /openshell-operator /usr/local/bin/openshell-operator
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/openshell-operator"]
