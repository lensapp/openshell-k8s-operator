# CLAUDE.md

Guidance for AI agents (and humans) working in this repository.

## What this is

A Kubernetes operator providing declarative CRDs over an OpenShell gateway's gRPC
control plane. It is a *thin front-end*: it translates custom resources into
gateway API calls and mirrors gateway state back into `.status`. It does **not**
reimplement the gateway.

Status: `OpenShellSandbox` (create/get/delete), `Provider` (static credentials
from a Secret, entitlement-checked, synced with a rotation watch), and `Policy`
(a reusable policy document validated by the gateway parser and applied to a
sandbox at creation via `policyRef`) reconcilers. Sandbox and Provider use
finalizer-based cleanup; Policy owns no gateway state, so it has none. Providers
v2 (profiles + gateway-managed OAuth2 refresh) is deferred to a separate future
CRD.

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
- Regenerate the CRD manifest after any change to `src/crd.rs`.

## Layout

- `src/crd.rs` — CRD types (pure schema; no gateway dependency).
- `src/gateway.rs` — `Gateway` trait + `SdkGateway`. The reconcilers depend on
  the trait so loops are unit-testable and the SDK is a swappable detail.
- `src/secret.rs` — Secret resolution + entitlement check (pure helpers tested).
- `src/policy.rs` — `PolicySpec` → proto `SandboxPolicy` conversion, delegating
  validation to `openshell-policy::parse_sandbox_policy` (pure, tested).
- `src/controllers/` — `mod.rs` (shared `Context` + `run`), `sandbox.rs`,
  `provider.rs`, `policy.rs`. One reconcile loop per resource.
- `src/error.rs` — operator error type.
- `src/main.rs` — entrypoint wiring.
- `src/bin/crdgen.rs` — CRD manifest generator (all kinds).

## Dependencies

`openshell-sdk` / `openshell-core` are git dependencies pinned to an exact rev on
NVIDIA/OpenShell `main`. Bump deliberately; treat the gRPC proto as an external
contract.

## Git

- Sign off commits (`git commit -s`); no co-author trailers.
- Never commit to `main`; always use a feature branch.
