<!--
SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Architecture

This document describes how the OpenShell Kubernetes operator is put together:
its guiding principle, the module layout, the reconcile model, and the runtime
concerns (auth, leader election, health) that surround the control loops. For
day-to-day usage see the [README](../README.md); for the gateway authentication
handshake see [operator-auth.md](operator-auth.md).

## Guiding principle: a thin front-end

The operator is a *thin front-end* over an OpenShell gateway's gRPC control
plane. It does exactly two things:

1. Translate custom resources (desired state) into gateway API calls.
2. Mirror gateway state back into each resource's `.status`.

It deliberately does **not** reimplement the gateway. Sandbox lifecycle, policy
enforcement, credential brokering, and compute placement all live in the
gateway. The operator's job is to make those capabilities *declarative* and
*Kubernetes-native* — nothing more. When a decision could be made either in the
operator or in the gateway, it belongs in the gateway, and the operator forwards
it.

This principle is why the CRD schema is intentionally close to the gateway proto
(see [Mapping to the proto](#mapping-to-the-gateway-proto)), and why the operator
holds no durable state of its own beyond what Kubernetes and the gateway already
persist.

## Workspace layout

The repository is a two-crate Cargo workspace:

- **`crates/operator`** — the operator itself (the controller manager binary).
- **`crates/issuer`** — a small static OIDC issuer used to authenticate the
  operator to the gateway. See [Authentication](#authentication).

Inside `crates/operator/src`:

| Module | Responsibility |
|---|---|
| `crd.rs` | CRD types — pure schema, no gateway dependency. |
| `gateway.rs` | The `Gateway` trait and its `SdkGateway` implementation. |
| `controllers/` | One reconcile loop per resource: `sandbox.rs`, `provider.rs`, `policy.rs`, plus shared `mod.rs`. |
| `secret.rs` | Secret resolution and the entitlement check (pure helpers). |
| `policy.rs` | `PolicySpec` → proto `SandboxPolicy`, delegating validation to the gateway's parser. |
| `conditions.rs` | Standard `.status.conditions` construction. |
| `volumes.rs` | PVC provisioning/retention helpers for sandbox volumes. |
| `leader.rs` | Lease-based leader election. |
| `health.rs` | Liveness/readiness HTTP probes. |
| `error.rs` | The operator error type. |
| `main.rs` | Entrypoint wiring. |
| `bin/crdgen.rs` | CRD manifest generator for all kinds. |

## The reconcile model

Each CRD has its own reconcile loop, but they share one `Context`:

```rust
pub struct Context {
    pub kube: Client,               // Kubernetes API access
    pub gateway: Arc<dyn Gateway>,  // the gateway, behind a trait
    pub recorder: Recorder,         // Kubernetes Events
}
```

`controllers::run` builds the context once and starts all three controllers on
the same Tokio runtime. Every loop follows the standard kube-rs shape: a
`reconcile` function returning an `Action` (requeue after N seconds) and an
`error_policy` that requeues on failure. Requeue cadence is uniform across
resources:

- `REQUEUE_INTERVAL` — 300s: steady-state resync (catch external drift).
- `TRANSITIONAL_REQUEUE_INTERVAL` — 10s: while a resource is still converging.
- `ERROR_REQUEUE_INTERVAL` — 15s: after a reconcile error.

Reconcile is idempotent: it computes desired state, compares it to observed
gateway state, applies the minimal change, and writes `.status`. Running it twice
with no external change is a no-op.

### Dependency inversion: the `Gateway` trait

The reconcilers depend on the `Gateway` **trait**, never on the SDK directly. In
production `Context.gateway` is an `SdkGateway` (wrapping `openshell-sdk`); in
tests it is a fake. This is the single most important seam in the codebase:

- The control-loop logic is unit-testable without a live gateway or a cluster.
- The SDK is a swappable detail — a proto bump or transport change is contained
  to `gateway.rs`.

This is dependency inversion applied to the one dependency that would otherwise
make the loops untestable.

## The three CRDs

All three are in API group `openshell.lenshq.io`, version `v1alpha1`, and expose
a `Ready` condition as a printer column.

### OpenShellSandbox

The core resource: one custom resource ⇄ one gateway sandbox. The reconciler
creates the sandbox on the gateway, mirrors its lifecycle `Phase`
(`Provisioning` → `Ready`, or `Failed`) into `.status`, and tears it down on
delete. Cleanup is guarded by the finalizer
`openshell.lenshq.io/sandbox-cleanup`, so the gateway sandbox is always deleted
before the Kubernetes object disappears.

A sandbox may reference an `OpenShellPolicy` (`spec.policyRef`) or inline one
(`spec.policy`), and may reference one or more `OpenShellProvider`s.

### OpenShellProvider

A named set of static credentials the gateway can use, sourced from a
Kubernetes `Secret`. The reconciler:

1. Resolves the referenced `Secret` and checks its **entitlement annotation**
   (`openshell.lenshq.io/allow-provider-ref: "true"`) — a Secret must opt in
   before its bytes can be forwarded to the gateway. This keeps arbitrary
   Secrets from being exfiltrated via a Provider reference.
2. Upserts the credentials to the gateway (idempotent).
3. **Watches** referenced Secrets, so a credential rotation triggers a resync of
   every Provider that points at the rotated Secret.

Cleanup is guarded by `openshell.lenshq.io/provider-cleanup`. Credential values
never appear on any custom resource — only the `SecretRef` does.

> Provider v2 (profiles + gateway-managed OAuth2 refresh) is deliberately
> deferred to a separate future CRD; this reconciler covers static credentials
> only.

### OpenShellPolicy

A reusable policy document, validated once by the gateway's own parser
(`openshell-policy::parse_sandbox_policy`) so a bad policy is rejected at admission
of the Policy rather than at sandbox creation. A Policy **owns no gateway state**:
it is just a validated document applied to a sandbox at creation via `policyRef`.
Consequently it has **no finalizer** — there is nothing to clean up.

## Convergence: recreate vs. update-in-place

Not every spec field is mutable on a live sandbox. The gateway's mutability
contract splits fields into two classes, and the reconciler honours that split:

- **Mutable in place.** Policy fields the gateway's `UpdateConfig` accepts on a
  running sandbox (network and inference freely, filesystem additively). When
  only these drift, the reconciler issues an in-place update — no disruption.
- **Immutable.** Everything else (image, resources, GPU, runtime class, labels,
  annotations, log level, landlock/process policy). A change here requires
  recreating the sandbox.

To detect immutable drift cheaply, the reconciler computes an
**immutable fingerprint** over the normalized set of immutable fields (a
`serde_json` render hashed with `DefaultHasher`). It stores that fingerprint and,
on each reconcile, recomputes and compares. A mismatch triggers recreate; a match
means at most an in-place policy update is needed.

Two properties matter for correctness:

- **Normalization.** The fingerprint hashes the *same* values the operator
  actually sends to the gateway (e.g. `gpuCount` is dropped when `gpu: false`;
  empty resource blocks collapse to absent). Hashing raw spec values instead
  would cause spurious recreates when a field is set but semantically empty.
- **Upgrade-safety.** New fingerprint keys are only added when their field is
  set, so introducing a new primitive does not change the fingerprint of
  existing sandboxes and does not trigger a mass recreate on operator upgrade.

### Volumes survive recreate

Sandbox volumes are backed by PVCs whose retention is controlled by
`spec.volumeRetention`. Recreate-to-converge is careful to preserve retained
volumes across the delete/create cycle, so an immutable-field change does not
silently discard a user's data. Provisioning and cleanup of these PVCs live in
`volumes.rs`.

## Status: conditions, phase, and events

Each resource reports state three ways, each for a different consumer:

- **`.status.conditions`** — the machine-readable truth. A standard `Ready`
  condition carries reconcile health and a reason; other conditions add detail.
  `conditions.rs` centralizes their construction.
- **`.status.phase`** — a coarse lifecycle label mirrored from the gateway
  (`Provisioning` / `Ready` / `Failed`) for at-a-glance `kubectl get` output.
  It is distinct from `Ready`, which is about reconcile health.
- **Kubernetes Events** — human-facing breadcrumbs via the shared `Recorder`
  (e.g. sandbox created, credentials rotated, recreate triggered).

## Mapping to the gateway proto

Because the operator is a thin front-end, the CRD schema tracks the gateway proto
closely. The sandbox mapping (`gateway.rs::create_sandbox_request`) is
representative:

- Scalar spec fields map directly onto `SandboxSpec` / `SandboxTemplate` fields
  (`log_level`, `runtime_class_name`, `labels`, `annotations`, …).
- `spec.gpuCount` becomes `GpuResourceRequirements { count }`, sent only when
  GPU is requested.
- `spec.resources` (`requests`/`limits` × `cpu`/`memory`) is rendered into the
  template's `resources` `google.protobuf.Struct`, whose nested shape matches the
  gateway's own `extract_typed_resources` parser
  (`{requests:{cpu,memory}, limits:{cpu,memory}}` with string values).

The `openshell-sdk` / `openshell-core` dependencies are pinned to an exact git
rev. The gRPC proto is treated as an **external contract**: bumps are deliberate,
and the mapping code is the one place that has to change when it moves.

## Authentication

The operator calls the gateway with a bearer token. Rather than depend on an
external identity provider, the workspace bundles a minimal static OIDC issuer
(`crates/issuer`) with two modes:

- **`mint`** (one-shot) — generates an RS256 signing key, mints the operator's
  admin JWT, and publishes the token `Secret` + JWKS `ConfigMap`. The private key
  lives only for the life of the process and is never persisted.
- **`serve`** (long-running) — serves the OIDC discovery document and JWKS from a
  mounted `ConfigMap`. It holds public material only and cannot sign.

The full handshake, and why it is split this way, is documented in
[operator-auth.md](operator-auth.md).

## Runtime concerns

### Leader election

For safe multi-replica rollout the operator supports leader election over a
`coordination.k8s.io/v1` `Lease` (hand-rolled in `leader.rs`, because the
existing helper crate pins an incompatible kube version). The core decision logic
is pure and unit-tested:

- `decide(holder, identity, unchanged_for) -> Renew | Acquire | Wait`
- lease expiry is measured from the *local* observation of `renewTime`, making it
  robust to clock skew between operator replicas and the API server.

Only the leader runs the reconcilers; on losing the lease the process reports
`LeadershipLost` and steps down. When leader election is disabled the operator
runs the controllers directly (single-replica). A Helm chart guard rejects
`replicaCount > 1` unless leader election is enabled.

### Health probes

`health.rs` serves two HTTP endpoints (via axum) for Kubernetes probes:

- `/healthz` (liveness) — always `200` while the process is up.
- `/readyz` (readiness) — `503` until startup completes, then `200`.

Readiness is marked **before** the leader-election campaign and does **not**
track leadership. This is deliberate: gating readiness on being the leader would
make non-leader replicas perpetually unready and can deadlock a rolling update.
Readiness stickiness is safe because the process exits on any fatal error rather
than flipping back to not-ready.

## Testing strategy

- The `Gateway` trait lets every reconcile path run against a fake gateway, so
  loop logic (create, drift → recreate, in-place update, cleanup, fingerprint
  stability) is covered by fast unit tests with no cluster.
- Pure helpers (`secret.rs` entitlement, `policy.rs` conversion, `leader.rs`
  `decide`/`observe`, fingerprint normalization) are tested directly.
- The gateway proto contract is validated at the mapping boundary
  (`gateway.rs` tests assert the request carries each primitive).
