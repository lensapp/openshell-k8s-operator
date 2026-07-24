# CLAUDE.md

Guidance for AI agents (and humans) working in this repository.

## What this is

A Kubernetes operator providing declarative CRDs over an OpenShell gateway's gRPC
control plane. It is a *thin front-end*: it translates custom resources into
gateway API calls and mirrors gateway state back into `.status`. It does **not**
reimplement the gateway.

Status: `OpenShellSandbox` (create/get/delete), `Provider` (static credentials
from a Secret, entitlement-checked, synced with a rotation watch), `Policy`
(a reusable policy document validated by the gateway parser and applied to a
sandbox at creation via `policyRef`), and `OpenShellWorkspace` (a cluster-scoped
gateway tenancy boundary with declarative membership, joined by sandboxes and
providers via `spec.workspace`) reconcilers. Sandbox, Provider, and Workspace
use finalizer-based cleanup; Policy owns no gateway state, so it has none.
Providers v2 (profiles + gateway-managed OAuth2 refresh) is deferred to a
separate future CRD.

The repo is a Cargo workspace with two crates: `crates/operator` (the operator)
and `crates/issuer` (a small static OIDC issuer — `mint` + `serve` subcommands —
that the Helm chart bundles to mint the operator's gateway bearer). The operator
authenticates to the gateway with an OIDC bearer, runs lease-based leader
election, and serves `/healthz` + `/readyz` probes.

## Build / test / lint

```bash
cargo build
cargo test
cargo clippy --all-targets   # pedantic + nursery; must be clean
cargo fmt --check
cargo run --bin crdgen > deploy/charts/openshell-operator/files/crds.yaml   # regenerate CRD manifests
```

## Conventions

- Follow OpenShell's Rust conventions (its `STYLEGUIDE.md`).
- SPDX license headers on every source file (Apache-2.0).
- Clippy `all` + `pedantic` + `nursery` at warn (see `Cargo.toml` + `clippy.toml`);
  keep the tree warning-free rather than sprinkling `#[allow]`.
- Public config/enum types are `#[non_exhaustive]`.
- Regenerate the CRD manifest after any change to `crates/operator/src/crd.rs`.

## Layout

A Cargo workspace with two crates.

- `crates/operator/` — the operator. CRD schema (`crd.rs`) is kept free of any
  gateway dependency. The reconcilers (`controllers/`, one loop per resource)
  depend on a `Gateway` trait (`gateway.rs`) so loops are unit-testable and the
  SDK is a swappable detail; conversion/validation helpers (`secret.rs`,
  `policy.rs`, `volumes.rs`) are pure and tested in isolation. Runtime concerns
  live in their own modules (`leader.rs` election, `health.rs` probes).
- `crates/issuer/` — the bundled OIDC issuer, one binary with `mint` (generate
  key, mint the operator's bearer, publish JWKS) and `serve` (OIDC discovery +
  JWKS) subcommands.

## Dependencies

`openshell-sdk` / `openshell-core` are git dependencies pinned to an exact rev on
NVIDIA/OpenShell `main`. Bump deliberately; treat the gRPC proto as an external
contract.
