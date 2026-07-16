# OpenShell Kubernetes Operator

Declarative, Kubernetes-native control over
[OpenShell](https://github.com/NVIDIA/OpenShell) sandboxes — manage them with
`kubectl apply` instead of talking to the gateway directly.

> **Status:** early development. `OpenShellSandbox` (create / get / delete with
> finalizer cleanup and status mirroring), `OpenShellProvider` (static credentials
> resolved from a Secret, synced to the gateway), and `OpenShellPolicy` (a reusable
> sandbox policy document applied at sandbox creation) resources are implemented.

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
reports the result in `.status.valid` / `.status.message`, so a bad policy
surfaces before any sandbox uses it.

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
