<!--
SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Operator → gateway authentication

Design for how the operator authenticates to the OpenShell gateway, and how a
single `helm install` stands up a working, secured deployment (with a
bring-your-own-gateway opt-out).

## Background: correcting an earlier premise

An earlier version of `PLAN.md` described the security model as an **in-pod
Envoy** that fronts a loopback-only gateway and forwards *only* sandbox-callable
gRPC methods, with `allowUnauthenticatedUsers=true` so the co-located operator
needs no credential. **That component does not exist upstream and is not needed.**
Investigation of the gateway source established:

- The upstream OpenShell chart puts **only the gateway** in its pod — no in-pod
  Envoy, no sidecar injection point. The `envoy-gateway-openshell.yaml` the plan
  cited is just a `GatewayClass` for the *cluster-level* Envoy Gateway, which
  routes **all** methods (no per-method allow-list).
- The gateway **already enforces per-method authorization in Rust at dispatch**
  (`auth/method_authz.rs` + `multiplex.rs`): a `Principal::Sandbox` is rejected
  before the handler unless the method is `is_sandbox_callable`. A sandbox
  **cannot** call `CreateProvider`/`DeleteSandbox`.
- The auth chain is **fail-closed**: a sandbox that drops its JWT gets `401`,
  **not** admin — *unless* `allow_unauthenticated_users=true`, which is the only
  thing that turns "no credential" into an admin principal.

So the proposed Envoy allow-list would only re-implement a check the gateway
already performs, and would exist solely to plug a hole (`allow_unauthenticated_users`)
we would be opening on purpose. We do neither.

## Decision

- **Do not** build a proxy; **do not** set `allow_unauthenticated_users`.
- Keep the gateway in its normal posture: network-exposed, TLS on, its own
  per-method authz enforcing the sandbox/user boundary.
- The operator authenticates as an **OIDC `User`** with an admin role, using a
  bearer token minted by a small **static OIDC issuer bundled in the chart**
  (no external IdP required).

The only mechanism that yields a `User`/admin principal without modifying the
gateway is OIDC — mTLS-user auth is a hard boot-time block under the Kubernetes
compute driver, and the built-in K8s-ServiceAccount authenticator only mints
`Sandbox` principals. Bundling a static issuer keeps it self-contained.

## Verified gateway facts (pin the design)

- **OIDC discovery is URL-based only** (`auth/oidc.rs`): the gateway GETs
  `{issuer}/.well-known/openid-configuration`, checks the doc's `issuer` matches
  config, then fetches `jwks_uri`. No static-JWKS-file option → a live JWKS
  endpoint is required.
- **`http://` issuer works** — it's a plain `reqwest` GET, no HTTPS enforcement.
- Token must be **RS256 / RSA**, header carries a **`kid`** matching a JWK in the
  set; `iss` and `aud` are validated; roles are read from a configurable claim
  path (`roles_claim`), and admin is granted when a role matches the gateway's
  configured `admin_role`.
- **No mTLS needed.** `cli.rs`: `require_client_auth = has_client_ca && !has_oidc`
  — with OIDC configured this is `false`, so the TLS listener does server-auth
  only (`with_no_client_auth()`, or `allow_unauthenticated()` if a client CA is
  present). The operator connects over server-TLS with just the bearer.
- SDK support: `openshell_sdk::AuthConfig::oidc(token)` sends a static bearer in
  the `authorization` header (no refresh needed for a long-lived token).

## Components (Cargo workspace)

```
crates/
  operator/   # today's crate: operator + crdgen bins, lib. Small auth change only.
  issuer/     # new binary, two subcommands:
              #   mint  — one-shot, holds the private key transiently
              #   serve — long-running, public JWKS only, cannot sign
```

The mint capability is confined to a short-lived Job; the always-on `serve` pod
never holds the private key.

## Flow

```
helm install
  │
  ├─ pre-install hook Job:  issuer mint
  │     • generate RSA keypair (RS256)
  │     • sign the operator JWT: iss=<issuer-svc URL>, aud=<audience>,
  │       roles:["openshell-admin"], no expiry, header kid=K
  │     • via the kube API, create-if-absent (idempotent — keeps the key stable
  │       across upgrades; regenerating would invalidate the live token + JWKS):
  │         Secret    <rel>-operator-token   { token }
  │         ConfigMap <rel>-oidc-jwks        { openid-configuration, jwks.json (public) }
  │     • needs a hook ServiceAccount + Role: secrets/configmaps get,create,patch
  │
  ├─ issuer serve  (Deployment + Service)
  │     • mounts the JWKS ConfigMap, serves /.well-known/openid-configuration + /keys
  │     • public-only, no private key
  │
  ├─ gateway (bundled subchart or BYO) configured:
  │     oidc.issuer   = https://<issuer-svc>.<ns>.svc:PORT   (http also accepted)
  │     oidc.audience = openshell-operator
  │     oidc.roles_claim = "roles";  authz.admin_role = "openshell-admin"
  │     TLS on, no client CA  → require_client_auth=false (server-TLS only)
  │
  └─ operator Deployment
        • mounts <rel>-operator-token; sets ClientConfig.auth = AuthConfig::oidc(token)
        • dials https://<gw-svc>…:8080 with ClientConfig.ca_cert = gateway CA
        • gateway validates the bearer → Principal::User(admin)
```

Sandboxes continue to authenticate with their gateway-minted JWTs; the gateway's
own per-method authz keeps them on the data plane. No proxy, no allow-list.

## Transport

The operator→gateway hop carries a long-lived admin bearer, so it defaults to
**server-TLS** (option ii): the operator trusts the gateway's CA (from the
gateway's cert-bootstrap Secret) and dials `https://`. This is cheap because OIDC
disables client-cert requirement — no mTLS. The gateway's server cert must carry
a SAN for the in-cluster Service DNS the operator dials (cert-bootstrap
`--server-san`/wildcard). A plaintext mode (`disable_tls=true`) stays available
behind a chart value for bare-bones dev, paired with a NetworkPolicy.

## Chart wiring

- `auth.mode: bundledOidc` (default) renders: the mint Job + its hook RBAC, the
  `issuer serve` Deployment/Service, and the gateway's OIDC config.
- `auth.mode: byo` renders none of the above; the operator points at an external
  gateway/IdP and consumes a token Secret the operator provides.
- Hook weights order it: **mint Job → serve / gateway / operator**.
- The Secret/ConfigMap the Job creates are not Helm-tracked (created at runtime),
  so they need an ownerReference or a cleanup hook. The token Secret holds an
  admin credential — keep its read-RBAC tight (operator SA only).

## Open follow-ups

- Confirm the gateway cert-bootstrap can emit a server cert with the operator's
  target Service-DNS SAN (else document `insecure_skip_verify` for dev only).
- Whether to bundle the upstream gateway chart as an OCI subchart for the
  batteries-included install, vs BYO gateway — tracked separately.
- Token rotation: v1 uses a long-lived token; if short-lived tokens are wanted
  later, the mint capability must become a running service (private key at rest),
  or use the SDK's `Refresh` trait against a real IdP.
