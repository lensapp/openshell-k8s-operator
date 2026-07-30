# OpenShell Kubernetes Operator

Declarative, Kubernetes-native control over [OpenShell](https://github.com/NVIDIA/OpenShell) sandboxes — manage them with `kubectl apply` instead of talking to the gateway directly.

> **Status:** early development. Five resources are implemented: `OpenShellSandbox` (full lifecycle — finalizer cleanup, provisioned volumes, in-place convergence, recreate-on-drift), `OpenShellProvider` (credentials from a Secret, synced as a static copy or gateway-minted refresh), `OpenShellPolicy` (a reusable policy applied at sandbox creation), `OpenShellWorkspace` (a cluster-scoped tenancy boundary with declarative membership), and `OpenShellProviderProfile` (a cluster-scoped, platform-scoped provider *type* definition).

## What it does

The operator is a thin front-end over the OpenShell gateway's gRPC API. You declare desired state as custom resources; the operator reconciles them into gateway calls and mirrors gateway state back into the resource `.status`. It does not reimplement the gateway.

## Install

### Prerequisite: Agent Sandbox

The OpenShell gateway provisions sandbox pods through the [Agent Sandbox](https://agent-sandbox.sigs.k8s.io) Kubernetes SIG project (`sandboxes.agents.x-k8s.io`). Its controller and CRDs are a **cluster-wide prerequisite** — install them once, before the chart, whether you use the bundled gateway or bring your own:

```bash
kubectl apply -f https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v0.5.2/sandbox.yaml
kubectl -n agent-sandbox-system rollout status deploy/agent-sandbox-controller
```

Without it the gateway comes up but its compute driver logs `no supported Agent Sandbox API version is available` and cannot create sandboxes. The chart does not bundle it: it is shared cluster infrastructure (a CRD + controller) whose lifecycle should not be tied to this release. If you install it *after* the gateway, restart the gateway so it re-detects the served API.

### Chart

One command installs a complete stack — gateway, OIDC issuer, and operator, already wired together:

```bash
helm install openshell deploy/charts/openshell-operator \
  --namespace openshell-system --create-namespace
```

By default (`gateway.bundled=true`) the chart pulls in the upstream OpenShell gateway as a subchart, stands up a small static OIDC issuer that mints the operator's admin bearer, and points the operator at the gateway over TLS — no external gateway, IdP, or manual config. The bundled gateway self-signs its TLS (no cert-manager), uses an ephemeral SQLite store, and takes a fixed in-cluster identity, so it's meant for one release per namespace and for dev/demo rather than production.

**Bring your own gateway** with `--set gateway.bundled=false --set gateway.endpoint=https://your-gateway:8080`. The chart then installs just the operator (and, by default, the issuer), and the install notes print the `issuer`/`audience`/`admin_role` values to configure your gateway to trust the issuer. Or set `auth.mode=byo` to mount your own token Secret (`auth.byo.tokenSecret`) instead of the bundled issuer. See [`docs/operator-auth.md`](docs/operator-auth.md) for the design.

`operator.deployStandalone=false` installs only the CRDs and RBAC (for embedding the operator container elsewhere; pair with `gateway.bundled=false`).

**High availability.** Leader election is on by default, so scaling the operator is safe: `--set replicaCount=3` runs three replicas that contend for a `coordination.k8s.io` Lease and let only the holder reconcile while the rest stand by, so a rolling update or node loss fails over cleanly. The container serves `/healthz` (liveness) and `/readyz` (readiness) on port 8080, wired as probes; readiness does not gate on leadership, so standbys report ready and never stall a rollout.

Raising `replicaCount` also brings a `PodDisruptionBudget` (`minAvailable: 1`, so a node drain cannot evict the last available replica) and a soft same-node spread — soft so a cluster with fewer nodes than replicas still schedules them all. Both follow `replicaCount` rather than a separate switch: at a single replica a budget has nothing to order and would only wedge drains, so it is not rendered (and asking for one there is refused at render time). Replace `topologySpreadConstraints` for a strict or zonal spread, and set `priorityClassName` to keep the operator off the eviction list under node pressure.

An ingress-only `NetworkPolicy` for the operator pods is available with `networkPolicy.enabled=true`, restricting reachability to the probe and webhook ports. It is off by default because a policy is silently inert under a CNI that does not enforce them. Egress is left alone on purpose: what the operator dials (API server, gateway, DNS) is cluster-specific, so pair it with your own egress policy.

Key values: `gateway.bundled`, `gateway.endpoint`, `gateway.caSecret`, `auth.mode`, `auth.oidc.*`, `image.repository` / `image.tag`, `logLevel`, `crds.install`, `operator.deployStandalone`, `replicaCount`, `leaderElection.enabled`, `podDisruptionBudget.enabled`, `topologySpreadConstraints`, `priorityClassName`, `networkPolicy.enabled`, `webhook.execConfinement.enabled`, `resources`.

## Example

A complete, self-contained setup: a credential Secret, an `OpenShellProvider` that binds it on the gateway, an `OpenShellPolicy` that constrains the sandbox, and an `OpenShellSandbox` that pulls both together via `providers` and `policyRef`.

```yaml
# Credentials live in a Secret, never on the CR. It opts in to being referenced.
apiVersion: v1
kind: Secret
metadata:
  name: anthropic-credentials
  annotations:
    openshell.lenshq.io/allow-provider-ref: "true"
stringData:
  ANTHROPIC_API_KEY: sk-ant-...
---
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellProvider
metadata:
  name: anthropic
spec:
  type: claude-code   # a gateway provider-profile id (exact, not an alias)
  credentialsSecretRef:
    name: anthropic-credentials   # keys: [] reads all keys
---
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellPolicy
metadata:
  name: restricted
spec:
  filesystem:
    includeWorkdir: true
    readOnly: ["/etc"]
  process:
    runAsUser: sandbox
  networkPolicies:
    claude_code:
      endpoints:
        - host: api.anthropic.com
          port: 443
---
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellSandbox
metadata:
  name: my-sandbox
spec:
  image: ghcr.io/nvidia/openshell-community/sandboxes/python:latest
  environment:
    LOG_LEVEL: debug
  providers:
    - anthropic       # the OpenShellProvider above
  policyRef: restricted   # the OpenShellPolicy above, applied at creation
  gpu: false
  gpuCount: 1             # GPUs to request when gpu is true (ignored otherwise)
  logLevel: info          # sandbox-runtime log level
  runtimeClassName: gvisor   # RuntimeClass requested from the compute platform
  resources:              # cpu/memory for the sandbox pod (Kubernetes quantities)
    requests:
      cpu: "500m"
      memory: 512Mi
    limits:
      cpu: "2"
      memory: 2Gi
  labels:                 # applied to the sandbox's pod
    team: platform
  annotations:
    example.com/owner: platform
  volumes:            # operator-provisioned, persists across recreation
    - name: work
      mountPath: /data
      claim:
        accessModes: ["ReadWriteOnce"]
        resources:
          requests:
            storage: 10Gi
  volumeRetention: Retain   # keep the PVC when the sandbox is deleted (default)
```

```console
$ kubectl apply -f example.yaml
$ kubectl get oss
NAME         READY   PHASE   SANDBOX    AGE
my-sandbox   True    Ready   3f2b...    30s

$ kubectl get osp anthropic
NAME        TYPE     READY   AGE
anthropic   claude   True    30s

$ kubectl get ospol restricted
NAME         READY   AGE
restricted   True    30s

$ kubectl wait --for=condition=Ready oss/my-sandbox
openshellsandbox.openshell.lenshq.io/my-sandbox condition met
```

The sections below cover each resource type in detail.

### Providers

An `OpenShellProvider` binds a credential set on the gateway. Credential values never live on the resource — the operator reads them from a Secret in the same namespace, which must opt in with the annotation `openshell.lenshq.io/allow-provider-ref: "true"` (see the `OpenShellProvider` and Secret in the example above). The operator watches the Secret, so external rotation (external-secrets, Vault) triggers a resync.

`credentialsSecretRef.keys` selects a subset of the Secret's keys (empty reads all of them), and `spec.config` passes non-secret settings (e.g. `region`) through to the gateway.

#### Credential handling

The same `OpenShellProvider` — no extra fields — gets the strongest credential handling the gateway supports for its `type`. At reconcile the operator reads the provider-type profile and picks per credential:

- **`Copied`** — the value is stored on the gateway as a static key (e.g. `claude-code`).
- **`Refresh`** — the gateway mints short-lived tokens from seed *material* you supply in the Secret (OAuth2 refresh / client-credentials, Google service-account JWT). The long-lived seed lives only in the gateway's refresh store, never as a stored static credential; the operator configures it once and re-seeds on Secret rotation — it does not mint tokens itself.
- **`Mixed`** — a multi-credential provider using both.

The operator surfaces the chosen tier in `.status.credentialMode` (the `Mode` print column). Nothing changes in the CR to get `Refresh` — only the Secret's contents differ, dictated by the provider type:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: vertex-credentials
  annotations:
    openshell.lenshq.io/allow-provider-ref: "true"
stringData:
  # google-vertex-ai's gcloud_adc_token refresh material — the operator routes
  # these to the gateway's refresh config, not into a stored static credential.
  client_id: "...apps.googleusercontent.com"
  client_secret: "..."
  refresh_token: "1//..."
---
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellProvider
metadata:
  name: vertex
spec:
  type: google-vertex-ai
  credentialsSecretRef:
    name: vertex-credentials
```

```console
$ kubectl get openshellproviders
NAME        TYPE               MODE      READY
anthropic   claude-code        Copied    True
vertex      google-vertex-ai   Refresh   True
```

If a refresh-capable credential is only partially provisioned (some required material missing), the operator falls back to a static copy and logs a warning rather than degrading silently. AWS STS assume-role is recognised but deferred (gateway-gated behind `providers_v2`), so it currently stays `Copied`.

### Policies

An `OpenShellPolicy` (see the example above) is a reusable sandbox policy document. An `OpenShellSandbox` names one via `spec.policyRef`; the operator resolves it and applies it when the sandbox is created. The high-value sections (`filesystem`, `landlock`, `process`) are typed; `networkPolicies` passes through opaquely, and the gateway's own parser validates it rather than the operator mirroring it here. The `OpenShellPolicy` reconciler validates the document and reports the result on its `Ready` condition (with the parser's diagnostic in the condition message), so a bad policy surfaces before any sandbox uses it.

For a one-off sandbox you can skip the separate resource and inline the same document directly under `spec.policy` instead of `spec.policyRef` (specify at most one of the two):

```yaml
kind: OpenShellSandbox
spec:
  policy:
    filesystem:
      includeWorkdir: true
    process:
      runAsUser: sandbox
```

Editing a policy — inline or a shared `OpenShellPolicy` — **is** retroactive: the operator watches referenced policies and reconverges every sandbox that names one. How it converges depends on the section (see [Updates and recreation](#updates-and-recreation)): `networkPolicies` and additive `filesystem` changes are pushed to the live sandbox in place, while `landlock`/`process` changes force a recreate, since the gateway holds those immutable. A shared policy that goes missing or invalid never tears down a running sandbox — the sandbox keeps its last-applied policy and the failure surfaces on its `Ready` condition.

### Workspaces

A workspace is the gateway's isolation boundary for sandboxes and providers. On the gateway its name is a single flat, global namespace, so `OpenShellWorkspace` is **cluster-scoped** — `metadata.name` *is* the gateway workspace name, and creating one or granting it members is cluster-level administration, not something a namespace tenant should self-serve. It is a tenant, like a `Namespace` or `StorageClass`, not a per-team resource.

```yaml
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellWorkspace
metadata:
  name: team-ml            # a DNS-1123 label, ≤19 chars, no consecutive hyphens
spec:
  labels:
    owner: ml-platform
  members:
    - subject: alice@example.com   # the OIDC `sub` claim
      role: Admin
    - subject: bob@example.com
      role: User
```

Namespaced `OpenShellSandbox` and `OpenShellProvider` join a workspace with `spec.workspace: team-ml`; an empty or omitted value uses the gateway's built-in `default` workspace (so existing resources are unaffected). `spec.workspace` is **immutable** — the gateway names its objects `{workspace}--{name}`, so a change would orphan the old object rather than move it; delete and recreate the resource to place it elsewhere.

Membership is authoritative when `spec.members` is present (even empty): the operator converges the gateway's members to exactly the listed set — adding, removing, and re-granting on a role change. Omit `spec.members` entirely to leave membership to out-of-band tooling; the operator then manages only the workspace's lifecycle. `spec.labels` are applied at creation only — the gateway has no workspace-update RPC, so later label edits do not converge.

Deletion is guarded by a finalizer. Because the gateway marks a workspace terminating *before* it checks for blockers — with no undelete — deleting one that still holds resources would permanently wedge it. So the operator refuses to delete a workspace while any `OpenShellSandbox` or `OpenShellProvider` still references it (surfacing `WorkspaceNotEmpty` on the `Ready` condition until it is emptied), and it never deletes the built-in `default`.

### Provider profiles

A provider *profile* is a provider **type** definition — the schema and template an `OpenShellProvider` is an instance of. The gateway ships built-in profiles for the well-known types (`claude`, `gitlab`, …); `OpenShellProviderProfile` lets a platform admin register a custom one declaratively. It is **cluster-scoped** and imported at the gateway's **platform** scope, so it is shared across every workspace — `metadata.name` *is* the profile id (lowercase kebab-case). Workspace-scoped profiles are deferred to a later revision.

```yaml
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellProviderProfile
metadata:
  name: acme-inference
spec:
  displayName: Acme Inference
  category: inference
  inferenceCapable: true
  credentials:                 # gateway's native snake_case schema
    - name: api_key
      env_vars: ["ACME_API_KEY"]
      required: true
  endpoints:
    - host: api.acme.example
      port: 443
```

Like `OpenShellPolicy`, only the stable spine (`displayName`, `description`, `category`, `inferenceCapable`) is typed; the large, fast-moving arrays (`credentials`, `endpoints`, `binaries`, `discovery`) pass through opaquely in the gateway's own `snake_case` schema, and the gateway's `openshell-providers` parser validates them at reconcile time rather than the operator mirroring them here. The reconciler reports the result on the `Ready` condition (with the parser's diagnostic in the message), so a bad profile surfaces before any provider uses it. It imports the profile on the gateway and updates it in place on change (optimistic-concurrency on the gateway's stored resource version, mirrored to `.status.resourceVersion`).

Deletion is guarded by a finalizer: the operator refuses to delete a profile while any `OpenShellProvider` still selects that `type` (surfacing `ProfileInUse` on the `Ready` condition until they are gone), since removing it would break their credential handling.

### Persistent volumes

`spec.volumes` gives a sandbox durable storage. For each entry the operator provisions a `PersistentVolumeClaim` (named `<sandbox>-<volume>`) from the embedded `claim` — the standard Kubernetes `PersistentVolumeClaimSpec`, so `storageClassName`, `accessModes`, `resources`, and `dataSource` (restore from a VolumeSnapshot or clone an existing PVC) are all available — and mounts it into the sandbox at `mountPath`.

The `OpenShellSandbox` owns the PVC, **not** the gateway sandbox underneath it. That is the point: the gateway treats a sandbox's image, policy, and other fields as immutable, so changing them means deleting and re-creating the sandbox — and because the PVC is anchored to the resource rather than the disposable sandbox, its data survives that recreation. Mounting a volume under `/sandbox` hands OpenShell's workspace persistence to it; mount elsewhere (e.g. `/data`, as above) to keep durable storage alongside the image-seeded workspace.

`volumeRetention` governs what happens to the PVCs when the `OpenShellSandbox` itself is deleted: `Retain` (default) keeps them so the data outlives the resource, `Delete` removes them. `volumeMode: Block` is rejected — the sandbox mounts a filesystem.

### Updates and recreation

The gateway treats much of a sandbox's spec as immutable on a running sandbox. When you edit an **immutable** field — `image`, `environment`, `gpu`/`gpuCount`, `logLevel`, `runtimeClassName`, `resources`, `labels`, `annotations`, the volume mounts, or the resolved policy's `landlock`/`process` (from `spec.policy` or a referenced `OpenShellPolicy`) — the operator converges by **deleting and recreating the gateway sandbox**. It tracks the applied fields as a hash in `.status.appliedSpecHash`, so it recreates only on a real change (and adopts a pre-existing sandbox without recreating it). During a recreate it emits a `Normal` `Recreating` event.

Operator-owned volumes are anchored to the `OpenShellSandbox`, so they survive the recreate and reattach by name — the whole reason for the volumes feature.

> ⚠️ **Recreation only preserves data on operator-owned volumes.** Deleting the gateway sandbox cascade-deletes anything the *gateway* owns — including its injected workspace PVC when the sandbox has **no** custom volume over `/sandbox`. Only the custom volumes above (referenced by `claimName`, with no owner reference) survive. To keep workspace state across an image or policy change, mount a volume at `/sandbox`; otherwise the workspace is rebuilt from the image on recreate.

The operator converges the **mutable** fields **in place**, without recreating:

- **`providers`** — editing `spec.providers` on a running sandbox attaches the newly-listed providers and detaches the removed ones, reconciled against what the gateway actually reports (so a manual detach is healed too).
- **The policy's `networkPolicies` and `filesystem`** — a change is pushed to the live sandbox via the gateway's `UpdateConfig`, tracked by a separate hash in `.status.appliedPolicyHash`, and announced with a `Normal` `PolicyUpdated` event. `filesystem` is **additively** mutable: adding read-only or read-write paths works, but the gateway rejects *removing* a path (or flipping `includeWorkdir`) — that failure surfaces on the `Ready` condition as `PolicyUpdateRejected`. To drop a filesystem path, recreate the sandbox.

## Status and events

Every resource reports reconcile health the standard way, so existing tooling just works:

- **`.status.conditions[]`** is the durable source of truth. Each resource carries a standard `Ready` condition (`metav1.Condition`, with `reason`, `message`, and `observedGeneration`), so `kubectl wait --for=condition=Ready`, Argo CD / Flux health assessment, and kstatus all understand it. On failure `Ready` goes `False` with a machine-readable `reason` (e.g. `VolumeProvisionFailed`, `SecretNotFound`, `PolicyConflict`) and a human `message`.
- **Kubernetes events** are the transient breadcrumb trail. Reconcile failures emit a `Warning` event against the resource, visible in `kubectl describe`. Events expire; conditions do not — so conditions, not events, are what automation should key on.

The `OpenShellSandbox` additionally mirrors the gateway's own lifecycle in `.status.phase` (a separate axis from `Ready`, much like `Pod.status.phase`).

## Confined `kubectl exec`

A sandbox's agent container runs privileged, so a plain `kubectl exec` lands as **root, outside** the per-process confinement the supervisor applies to the workload — bypassing the sandbox. An optional admission webhook, served by the operator, closes that: a mutating webhook on `pods/exec` rewrites the command to re-enter the sandbox via the supervisor (dropping to the sandbox user under Landlock, policy re-derived from the live gateway), and a validating webhook denies `kubectl attach` / `kubectl debug`, which would otherwise sidestep it. It works in the default topology — no privileged-container change needed.

`webhook.execConfinement.enabled` defaults to `gateway.bundled`: **on** for the bundled install (the operator knows sandboxes land in the release namespace and scopes the webhook there — nothing to label), **opt-in** for a bring-your-own gateway (enable it and label the sandbox namespaces `openshell.lenshq.io/exec-confinement=enabled`). It is `failurePolicy: Fail` (fail-closed: an operator outage denies exec into confined namespaces rather than reopening a root shell) and self-manages its serving cert — no cert-manager. See [`docs/exec-confinement.md`](docs/exec-confinement.md) for the design.

## Architecture

The operator is a thin, declarative front-end over the gateway's gRPC control plane: it translates custom resources into gateway calls and mirrors gateway state back into `.status`. See [`docs/architecture.md`](docs/architecture.md) for the full design — module layout, the reconcile model, drift detection, and the runtime concerns.

It authenticates to the gateway as an OIDC `User` (admin) with a bearer token minted by a small static OIDC issuer bundled in the chart — no external identity provider. The gateway keeps its normal posture: it enforces per-method authorization itself (fail-closed), so sandboxes, which present gateway-minted JWTs, stay confined to the data plane without any extra proxy. See [`docs/operator-auth.md`](docs/operator-auth.md) for the full design.

## Development

```bash
cargo build
cargo test
cargo run --bin openshell-operator   # against the current kubecontext; expects a
                                      # gateway at $OPENSHELL_GATEWAY_ENDPOINT
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full verification gate, the pinned toolchain, the git hooks, CRD regeneration, and commit conventions.

`openshell-sdk`, `openshell-core`, `openshell-policy`, and `openshell-providers` are consumed as git dependencies pinned to an NVIDIA/OpenShell release tag — the same release the chart pins as its bundled gateway subchart, so the client crates and the gateway they talk to always come from one upstream release. Bump both pins together.

### Releases

[release-please](https://github.com/googleapis/release-please) automates releases from the Conventional Commit history. It keeps a release PR open that bumps the chart `version` + `appVersion` and updates `CHANGELOG.md`; merging it tags `vX.Y.Z` and publishes, all to GHCR:

- `ghcr.io/lensapp/openshell-operator:X.Y.Z` and `openshell-issuer:X.Y.Z` (multi-arch `linux/amd64` + `linux/arm64`), plus `:latest`.
- the Helm chart at `oci://ghcr.io/lensapp/charts/openshell-operator` (version `X.Y.Z`).

Crate versions in `Cargo.toml` are internal and intentionally left untouched by releases. `feat:` bumps the minor, `fix:` the patch (breaking changes stay within `0.x` until a deliberate `1.0.0`).

## License

Apache-2.0 — see [LICENSE](LICENSE).
