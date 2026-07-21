# Contributing to the OpenShell operator

Thanks for your interest in the OpenShell Kubernetes operator.

This repository is a Cargo workspace with two crates — `crates/operator` (the
operator) and `crates/issuer` (the bundled static OIDC issuer) — plus the Helm
chart under `deploy/charts/`. [`CLAUDE.md`](CLAUDE.md) at the repo root is the
authoritative guide to what this is, its layout, and the project conventions;
this file is the short on-ramp.

## Development Setup

The Rust toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml)
(mirrored in [`mise.toml`](mise.toml) for mise users), so `rustup` installs the
pinned version automatically on first build. Match it locally — nursery lints
like `cognitive_complexity` drift between compiler versions, so an unpinned newer
toolchain can pass locally yet fail CI.

Wire the repository hooks into your checkout (one-time; `core.hooksPath` is not
stored in the repo):

```bash
git config core.hooksPath .githooks
```

That enables a pre-commit `fmt` + `clippy` gate and a `commit-msg` Conventional
Commits check. Fast inner loop:

```bash
cargo build
cargo test
cargo run --bin openshell-operator   # runs against the current kubecontext
```

## The Verification Gate

Run the same checks CI does before opening a pull request:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings   # pedantic + nursery; keep it clean
cargo test --all
```

Clippy runs `all` + `pedantic` + `nursery` at warn (see `Cargo.toml` +
`clippy.toml`); keep the tree warning-free rather than sprinkling `#[allow]`.

If you change the CRD schema (`crates/operator/src/crd.rs`), regenerate the
manifest and commit the result — CI fails if it is stale:

```bash
cargo run --bin crdgen > deploy/charts/openshell-operator/files/crds.yaml
```

Chart changes are linted and rendered by CI (`helm lint` + `helm template`
across every posture); run them locally with `azure/setup-helm` or your own
`helm` if you touch `deploy/charts/`.

## Testing

New behavior comes with tests. The codebase is structured so the logic is
testable without a cluster or a live gateway:

- **Pure helpers** (`secret.rs`, `policy.rs`, `volumes.rs`, `credentials.rs`) are
  unit-tested in isolation in `#[cfg(test)] mod tests`.
- **Reconcilers** (`controllers/`) depend on a `Gateway` trait and are tested
  against a fake gateway, so a loop's behavior is asserted without the SDK.

## Pull Request Guidelines

- Keep changes small and focused.
- Add or update tests for every behavior change.
- Keep SPDX license headers (`Apache-2.0`) on every source file.
- Public config/enum types are `#[non_exhaustive]`.
- Update documentation when you change public behavior, configuration, or the
  CRD schema.

## Commit Style

Conventional Commits are enforced (locally by the `commit-msg` hook, in CI by the
`commits` job) because releases are derived from them by release-please — only
`feat:` and `fix:` (and a `!` / `BREAKING CHANGE`) cut a release. Sign off your
commits (`git commit -s`). Do not add a `Co-Authored-By` trailer or a
generated-with footer.

- `feat: add Policy reconciler`
- `fix: keep the prior synced hash on a failed sync`
- `docs: document the credential ladder in the README`
- `test: cover the refresh-material planner`
- `ci: add the multi-arch release workflow`

## Security-Sensitive Changes

Changes touching credential resolution, the entitlement check, the
Secret-to-gateway path, gateway authentication, or the operator's RBAC should
include a short explanation of the security impact in the pull request
description. For vulnerability reports, follow [SECURITY.md](SECURITY.md).
