# OpenShell Kubernetes Operator

Declarative, Kubernetes-native control over
[OpenShell](https://github.com/NVIDIA/OpenShell) sandboxes — manage them with
`kubectl apply` instead of talking to the gateway directly.

> **Status:** early development. `OpenShellSandbox` (create / get / delete with
> finalizer cleanup and status mirroring), `Provider` (static credentials
> resolved from a Secret, synced to the gateway), and `Policy` (a reusable
> sandbox policy document applied at sandbox creation) resources are implemented.

## What it does

The operator is a thin front-end over the OpenShell gateway's gRPC API. You
declare desired state as custom resources; the operator reconciles them into
gateway calls and mirrors gateway state back into the resource `.status`. It
does not reimplement the gateway.

## Example

A complete, self-contained setup: a credential Secret, a `Provider` that binds
it on the gateway, a `Policy` that constrains the sandbox, and an
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
kind: Provider
metadata:
  name: anthropic
spec:
  type: claude
  credentialsSecretRef:
    name: anthropic-credentials   # keys: [] reads all keys
---
apiVersion: openshell.lenshq.io/v1alpha1
kind: Policy
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
    - anthropic       # the Provider above
  policyRef: restricted   # the Policy above, applied at creation
  gpu: false
```

```console
$ kubectl apply -f example.yaml
$ kubectl get oss
NAME         PHASE   SANDBOX    AGE
my-sandbox   Ready   3f2b...    30s

$ kubectl get osp anthropic
NAME        TYPE     PHASE   AGE
anthropic   claude   Ready   30s

$ kubectl get ospol restricted
NAME         VALID   AGE
restricted   true    30s
```

The three resource types are covered in detail below.

### Providers

A `Provider` binds a credential set on the gateway. Credential values never live
on the resource — they are read from a referenced Secret in the same namespace,
which must opt in with the annotation `openshell.lenshq.io/allow-provider-ref: "true"`
(see the `Provider` and Secret in the example above). The operator watches the
Secret, so external rotation (external-secrets, Vault) triggers a resync.

`credentialsSecretRef.keys` selects a subset of the Secret's keys (empty reads
all of them), and `spec.config` passes non-secret settings (e.g. `region`)
through to the gateway.

`Provider` covers static credentials only. Providers v2 (profiles + gateway-managed
OAuth2 refresh) is a separate, future resource.

### Policies

A `Policy` (see the example above) is a reusable sandbox policy document. An
`OpenShellSandbox` names one via `spec.policyRef`; the operator resolves it and
applies it when the sandbox is created. The high-value sections (`filesystem`,
`landlock`, `process`) are typed; `networkPolicies` is passed through opaquely,
validated by the gateway's own policy parser rather than mirrored here. The
`Policy` reconciler validates the document and reports the result in
`.status.valid` / `.status.message`, so a bad policy surfaces before any sandbox
uses it.

Because `filesystem`, `landlock`, and `process` are immutable on a running
sandbox, editing a `Policy` only affects sandboxes created afterwards.

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
