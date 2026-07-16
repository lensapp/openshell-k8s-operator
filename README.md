# OpenShell Kubernetes Operator

Declarative, Kubernetes-native control over
[OpenShell](https://github.com/NVIDIA/OpenShell) sandboxes — manage them with
`kubectl apply` instead of talking to the gateway directly.

> **Status:** early development. `OpenShellSandbox` (create / get / delete with
> finalizer cleanup, operator-provisioned persistent volumes, recreate-on-drift
> for immutable fields, and standard `Ready` conditions + events),
> `OpenShellProvider` (static credentials resolved from a Secret, synced to the
> gateway), and `OpenShellPolicy` (a reusable sandbox policy document applied at
> sandbox creation) resources are implemented.

## What it does

The operator is a thin front-end over the OpenShell gateway's gRPC API. You
declare desired state as custom resources; the operator reconciles them into
gateway calls and mirrors gateway state back into the resource `.status`. It
does not reimplement the gateway.

## Example

A complete, self-contained setup: a credential Secret, an `OpenShellProvider` that
binds it on the gateway, an `OpenShellPolicy` that constrains the sandbox, and an
`OpenShellSandbox` that pulls both together via `providers` and `policyRef`.

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
  type: claude
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

The three resource types are covered in detail below.

### Providers

An `OpenShellProvider` binds a credential set on the gateway. Credential values
never live on the resource — they are read from a referenced Secret in the same
namespace, which must opt in with the annotation
`openshell.lenshq.io/allow-provider-ref: "true"` (see the `OpenShellProvider` and
Secret in the example above). The operator watches the Secret, so external
rotation (external-secrets, Vault) triggers a resync.

`credentialsSecretRef.keys` selects a subset of the Secret's keys (empty reads
all of them), and `spec.config` passes non-secret settings (e.g. `region`)
through to the gateway.

`OpenShellProvider` covers static credentials only. Providers v2 (profiles +
gateway-managed OAuth2 refresh) is a separate, future resource.

### Policies

An `OpenShellPolicy` (see the example above) is a reusable sandbox policy
document. An `OpenShellSandbox` names one via `spec.policyRef`; the operator
resolves it and applies it when the sandbox is created. The high-value sections
(`filesystem`, `landlock`, `process`) are typed; `networkPolicies` is passed
through opaquely, validated by the gateway's own policy parser rather than
mirrored here. The `OpenShellPolicy` reconciler validates the document and
reports the result on its `Ready` condition (with the parser's diagnostic in
the condition message), so a bad policy surfaces before any sandbox uses it.

For a one-off sandbox you can skip the separate resource and inline the same
document directly under `spec.policy` instead of `spec.policyRef` (specify at
most one of the two):

```yaml
kind: OpenShellSandbox
spec:
  policy:
    filesystem:
      includeWorkdir: true
    process:
      runAsUser: sandbox
```

The operator applies a policy only when the sandbox is created — it is never
re-pushed to a running sandbox (and the gateway rejects any change to
`filesystem`, `landlock`, or `process` on a live sandbox anyway). So editing an
`OpenShellPolicy` is not retroactive: it affects only sandboxes created
afterwards. To move an existing sandbox onto new static policy, delete and
re-create it.

### Persistent volumes

`spec.volumes` gives a sandbox durable storage. For each entry the operator
provisions a `PersistentVolumeClaim` (named `<sandbox>-<volume>`) from the
embedded `claim` — the standard Kubernetes `PersistentVolumeClaimSpec`, so
`storageClassName`, `accessModes`, `resources`, and `dataSource` (restore from a
VolumeSnapshot or clone an existing PVC) are all available — and mounts it into
the sandbox at `mountPath`.

The PVC is owned by the `OpenShellSandbox`, **not** by the gateway sandbox
underneath it. That is the point: the gateway treats a sandbox's image, policy,
and other fields as immutable, so changing them means deleting and re-creating
the sandbox — and because the PVC is anchored to the resource rather than the
disposable sandbox, its data survives that recreation. Mounting a volume under
`/sandbox` hands OpenShell's workspace persistence to it; mount elsewhere (e.g.
`/data`, as above) to keep durable storage alongside the image-seeded workspace.

`volumeRetention` governs what happens to the PVCs when the `OpenShellSandbox`
itself is deleted: `Retain` (default) keeps them so the data outlives the
resource, `Delete` removes them. `volumeMode: Block` is rejected — the sandbox
mounts a filesystem.

### Updates and recreation

The gateway treats most of a sandbox's spec as immutable on a running sandbox.
When you edit an **immutable** field — `image`, `environment`, `gpu`, or the
inline policy's `landlock`/`process` — the operator converges by **deleting and
recreating the gateway sandbox**. It tracks the applied fields as a hash in
`.status.appliedSpecHash`, so it recreates only on a real change (and adopts a
pre-existing sandbox without recreating it). During a recreate it emits a
`Normal` `Recreating` event.

Operator-owned volumes are anchored to the `OpenShellSandbox`, so they survive
the recreate and reattach by name — the whole reason for the volumes feature.

> ⚠️ **Recreation only preserves data on operator-owned volumes.** Deleting the
> gateway sandbox cascade-deletes anything the *gateway* owns — including its
> injected workspace PVC when the sandbox has **no** custom volume over
> `/sandbox`. Only the custom volumes above (referenced by `claimName`, with no
> owner reference) survive. To keep workspace state across an image or policy
> change, mount a volume at `/sandbox`; otherwise the workspace is rebuilt from
> the image on recreate.

What does **not** trigger a recreate: `providers`, `networkPolicies`, and
`filesystem` (all mutable on a live sandbox), and edits to a referenced
`OpenShellPolicy` (applying a shared policy is deliberately not retroactive —
only the sandbox's own spec drives recreation). Of these, **`providers` are
converged in place** — editing `spec.providers` on a running sandbox attaches
the newly-listed providers and detaches the removed ones, reconciled against
what the gateway actually reports (so a manual detach is healed too). In-place
convergence of the policy's mutable fields (`networkPolicies`, additive
`filesystem`) is the next step.

## Status and events

Every resource reports reconcile health the standard way, so existing tooling
just works:

- **`.status.conditions[]`** is the durable source of truth. Each resource
  carries a standard `Ready` condition (`metav1.Condition`, with `reason`,
  `message`, and `observedGeneration`), so `kubectl wait --for=condition=Ready`,
  Argo CD / Flux health assessment, and kstatus all understand it. On failure
  `Ready` goes `False` with a machine-readable `reason` (e.g.
  `VolumeProvisionFailed`, `SecretNotFound`, `PolicyConflict`) and a human
  `message`.
- **Kubernetes events** are the transient breadcrumb trail. Reconcile failures
  emit a `Warning` event against the resource, visible in `kubectl describe`.
  Events expire; conditions do not — so conditions, not events, are what
  automation should key on.

The `OpenShellSandbox` additionally mirrors the gateway's own lifecycle in
`.status.phase` (a separate axis from `Ready`, much like `Pod.status.phase`).

## Architecture

The operator and gateway run in the **same pod**. The gateway binds to loopback,
so the operator reaches the control plane over `127.0.0.1` with no auth, while an
Envoy proxy exposes **only** the sandbox data-plane methods to sandbox pods. This
keeps the control plane private to the operator without an external identity
provider.

## Development

```bash
cargo build
cargo test
cargo clippy --all-targets   # pedantic + nursery; keep it clean
cargo fmt --check

# Regenerate the CRD manifests from the Rust types
cargo run --bin crdgen > deploy/crds/crds.yaml

# Run against the current kubecontext. Expects a gateway at
# $OPENSHELL_GATEWAY_ENDPOINT (default http://127.0.0.1:8080).
cargo run --bin openshell-operator
```

`openshell-sdk` and `openshell-core` are consumed as git dependencies pinned to
an exact revision of NVIDIA/OpenShell `main`.

## License

Apache-2.0.
