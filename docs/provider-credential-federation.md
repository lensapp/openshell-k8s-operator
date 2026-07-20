<!--
SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Provider credential federation

**Status:** design exploration. Captures the analysis behind how the operator
*should* deliver provider credentials, so the decision is recorded before any
milestone commits to it.

> **What actually shipped (partial):** a gateway-driven credential ladder, not
> the SPIFFE-broker federation this note explores. The reconciler auto-selects
> per credential from the provider-type profile: gateway-minted refresh
> (`ConfigureProviderRefresh`) when the Secret supplies the required seed
> material, static copy otherwise — surfaced as `.status.credentialMode`
> (`Copied` / `Refresh` / `Mixed`). See `crates/operator/src/credentials.rs`.
> The zero-seed federation (a tier-3 identity token grant) below remains
> unimplemented.

## The problem

Today an `OpenShellProvider` resolves a Kubernetes `Secret` and **pushes the
credential values to the gateway** via `CreateProvider`/`UpdateProvider`. The
gateway persists them and ships them to sandboxes. The secret therefore lives in
two places — the Kubernetes `Secret` and the gateway's store — kept in sync only
by the operator's rotation watch.

Two things make this worth improving:

1. **Duplication breaks the enterprise secret workflow.** Platform teams sync
   credentials from Vault/OpenBao into Kubernetes `Secret`s (e.g. via External
   Secrets Operator). Copying those *again* into OpenShell means the authoritative
   secret manager is no longer the single source.
2. **The gateway stores credentials in plaintext.** The gateway's store is
   SQLite with no envelope/at-rest encryption; `Provider.credentials`
   (`map<string,string>`, marked `[(secret)=true]` in `datamodel.proto`) is
   persisted as plaintext. Eliminating that copy removes a real at-rest exposure.

The goal: **stop copying secrets into the gateway** without weakening the
isolation the sandbox model already provides.

## How OpenShell handles credentials today (ground truth)

These facts, verified against the pinned upstream checkout, shape every option.

- **The provider API is value-based.** `datamodel.proto Provider.credentials` is
  an inline map; there is no reference/pointer field. `CreateProvider` /
  `UpdateProvider` take the values.
- **Resolution and injection already happen sandbox-side.** The agent process
  receives credential **placeholders** (`openshell:resolve:env:…`,
  `openshell-core/src/secrets.rs`), never the raw value. The supervisor's egress
  **proxy** substitutes the real credential just-in-time on allowed traffic
  (header/query/path rewriting, plus AWS SigV4 re-signing). The value reaches the
  supervisor from the gateway via `GetSandboxSettings`. The one gateway-side
  resolution point is `resolve_provider_environment_with_catalog`
  (`openshell-server/src/grpc/provider.rs`).
- **SPIFFE token grants already exist, end to end.** A provider profile credential
  can carry a `token_grant` (`ProviderCredentialTokenGrant` in `openshell.proto`)
  holding only non-secret metadata (`token_endpoint`, `audience`, `scopes`). The
  supervisor's proxy fetches the sandbox's SPIFFE JWT-SVID and exchanges it at the
  token endpoint for a short-lived token, caches it, and injects it
  (`openshell-supervisor-network/src/token_grant.rs` +
  `l7/token_grant_injection.rs`). The Kubernetes driver already mounts the SPIFFE
  Workload API socket into sandboxes when `provider_spiffe_enabled` is set. **No
  secret value passes through the gateway on this path.**
- **Injection honors placement.** `token_grant_injection.rs` supports
  `auth_style: bearer` (→ `Authorization: Bearer <v>`) *and* `auth_style: header`
  with a custom `header_name` (→ `<header_name>: <v>`, value verbatim). So the
  transport can deliver a non-Bearer, static-shaped credential such as
  `x-api-key`.
- **Built-in profiles declare placement + endpoints.** e.g.
  `providers/claude-code.yaml` declares `api_key` with
  `auth_style: header, header_name: x-api-key` and endpoints `api.anthropic.com`,
  etc. The operator can reuse this metadata rather than inventing it.
- **OpenShell has no first-class multi-tenancy.** Providers/sandboxes/policies are
  flat, name-addressed objects in one store; authz (`auth/authz.rs`) is role
  (Admin/User) + optional per-method scope, not tenant/namespace scoped; the k8s
  driver targets a single namespace. The de-facto tenancy boundary is *one gateway
  per tenant*.

## Options considered

### A. Gateway reads the Secret directly (`secretRef` on the provider)

This is the shape proposed in upstream issue
[NVIDIA/OpenShell#1882](https://github.com/NVIDIA/OpenShell/issues/1882): the
provider carries a `{namespace, name, key}` reference and the **gateway** resolves
the Secret at sandbox startup.

- **Cost:** proto change; a runtime kube client on the serving path; and —
  decisively — cross-namespace `get secrets` RBAC plus an authorization/tenancy
  model to police it. The gateway's flat store can't even express object ownership,
  so this means granting the most-trusted component broad Secret read while its
  own model has no tenancy. Maximum blast radius where the security model is
  weakest.
- **Verdict:** rejected as the strategic target. Viable only as a
  *same-namespace-only* v1 (gateway reads Secrets in its own namespace), which
  fits the one-gateway-per-tenant reality but does not serve the multi-tenant
  ESO use case that motivates #1882.

### B. Pod-local Secret mount, resolved in the supervisor

Deliver the Secret to the supervisor via a projected volume / CSI mount.

- **Cost / blocker:** the k8s driver runs all sandboxes in a single namespace, so
  a projected mount can't reach tenant namespaces — the operator would have to
  *copy* each Secret into the sandbox namespace, spreading it with no net win.
  Mounting into the pod also risks the untrusted agent reading it, unless confined
  to the sidecar container (only some topologies).
- **Verdict:** rejected for the shared-namespace driver model.

### C. Abstract external credential resolver (gateway calls out on demand)

Add an RPC by which the gateway asks an external service to resolve a provider's
credentials at sandbox start; the operator implements it using the RBAC it
already has; the value is in-memory only, never persisted.

- **Upside:** gateway becomes a broker, not a store; authz decision moves to the
  k8s-native operator; backend-agnostic; a natural sibling to the interceptor's
  existing `SnapshotProviderProfiles`.
- **Cost:** a new gateway RPC (upstream change), and it promotes the operator to a
  data-plane dependency on sandbox restart.
- **Verdict:** the right shape *if* an upstream change is required — but see the
  recommendation, which achieves the same properties with **no** upstream change.

### D. SPIFFE token grant (identity instead of a stored secret)

Use the existing `token_grant` path: the sandbox exchanges its SPIFFE identity for
a short-lived token at a token service. No secret stored anywhere.

- **Upside:** for any OAuth2/OIDC-federatable provider (internal APIs behind
  Keycloak, cloud APIs via workload-identity federation), the credential is
  eliminated from *every* store — gateway and Kubernetes alike. Already
  implemented; shippable with no upstream change.
- **Limit:** only works where the provider (or a fronting token service) accepts a
  SPIRE-issued JWT-SVID. A raw third-party SaaS key (e.g. a direct Anthropic
  `x-api-key`) is the irreducible **static residue**.

## Recommendation: transparent federation behind `credentialsSecretRef`

Combine C's properties with D's transport, and **hide it behind the existing
CRD**. The insight: the operator can act as the token service itself. Because
`token_grant` calls an *arbitrary* `token_endpoint`, the operator can be that
endpoint and return the resolved Secret value — gated on the sandbox's SPIFFE
identity. This needs **no upstream change**.

The user-facing CRD does not change at all:

```yaml
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellProvider
metadata:
  name: anthropic
spec:
  type: claude
  credentialsSecretRef:
    name: anthropic-credentials
```

`credentialsSecretRef` stops meaning *"copy this to the gateway"* and starts
meaning *"federate this through the operator broker."* At reconcile time the
operator:

1. **Looks up the provider-type profile** (built-in or supplied) to obtain the
   credential placement and endpoints. For `type: claude` that yields
   `auth_style: header, header_name: x-api-key` and `api.anthropic.com` from
   `claude-code.yaml` — no guessing.
2. **Registers a credential-less, token-grant profile/instance** with the gateway:
   same placement, but the credential's `token_grant.token_endpoint` points at the
   operator's own broker. **Nothing is written to the gateway's credential store.**
3. **Serves the broker.** The sandbox proxy presents its SPIFFE JWT-SVID; the
   operator validates it against the SPIRE trust bundle, checks that this sandbox
   is authorized for this provider, resolves the referenced `Secret` (with the
   RBAC it already holds), and returns the value as the token response. The proxy
   caches it and injects `x-api-key: <value>` on matching traffic.

Placement is **identical** on the static and federated paths (`x-api-key` either
way); the operator only swaps *where the value comes from*. That is what makes the
rewrite transparent.

### What this buys

- **The gateway is entirely out of the secret path** — no plaintext-at-rest copy,
  no gateway Secret RBAC, no tenancy model forced into the gateway.
- **The operator uses only the RBAC it already has**, and exercises it *only* in
  response to a SPIFFE-authenticated, authorized request.
- **The authorization map comes for free** — "which SPIFFE ID may fetch which
  provider" is derivable from the `OpenShellSandbox.spec.providers` bindings the
  operator already reconciles.
- **Rotation is automatic** within the proxy cache TTL — the operator resolves the
  current Secret on each exchange; no sync loop.

### Honest boundaries

- **Storage/distribution, not lifetime, for static keys.** For a genuinely
  federatable provider (Keycloak-fronted), tokens are short-lived. For a static
  key fronted by the operator (Anthropic), the operator federates *storage and
  distribution* but the key stays long-lived — it sits transiently in the proxy
  cache, which is no worse than today's in-memory resolution. Only the provider
  itself can make its credential short-lived.
- **Single-header-key providers rewrite cleanly.** Multi-value credential sets
  (e.g. AWS access key + secret + session for SigV4) don't collapse into one
  `token_grant` injection; those stay on the copy or SigV4 path.
- **It is a deployment mode, not magic.** Federation requires SPIRE in the cluster
  and the operator broker running (HA, hardened: mTLS, assertion expiry/replay
  checks, no secret logging, rate limiting). The operator must **detect the
  prerequisites and fall back to the copy path — or surface a clear status
  condition — rather than fail silently** into a no-credentials sandbox. A
  `Provider` condition such as `CredentialMode: Federated | Copied` with a reason
  keeps the transparent switch debuggable.

### Where #1882 fits

#1882's `kubernetesSecret` source is best understood as the **no-SPIRE built-in
fallback** — the degenerate case for shops that don't run SPIRE and accept
gateway-side, same-namespace resolution. And for #1882's own motivating
environment (Vault/OpenBao behind ESO), the token service can be **Vault
directly**: point `token_grant.token_endpoint` at Vault's JWT/OIDC auth with
SPIFFE as the identity, and skip the ESO→Secret sync, the gateway store, *and* the
operator broker. The operator broker is the option for when no such token service
already exists.

## Suggested sequencing

1. **v1 — token grant, no upstream change.** Expose a token-grant variant on
   `OpenShellProvider` (and/or the transparent `credentialsSecretRef` rewrite for
   known single-key provider types). Enable the driver's `provider_spiffe_enabled`
   in the chart. Eliminates the stored secret for every federatable provider
   today. Keep the copy path as a labeled stopgap for the static residue.
2. **v1.5 — at-rest encryption (small upstream ask).** Champion envelope
   encryption of `Provider.credentials` in the gateway store — no tenancy model
   needed — to cover the residue while federation matures.
3. **v2 — external resolver (only if the static residue is material).** If enough
   providers can't federate, contribute option C (a credential-resolution RPC on
   the interceptor contract), not option A.

## Open questions

- **How much of the real provider set is federatable vs. raw keys?** This gates
  whether v1 is worth building at all. Size it before committing.
- **Broker availability SLO.** Federation puts the operator on the sandbox
  restart/reschedule path. Acceptable given the operator is already critical infra,
  but the broker must be HA and the failure mode explicit (fail-closed for
  credential-bearing bindings).
- **SPIRE dependency.** Is SPIRE already run in target clusters, or is standing it
  up part of the cost?
