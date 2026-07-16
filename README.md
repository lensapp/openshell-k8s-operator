# OpenShell Kubernetes Operator

Declarative, Kubernetes-native control over
[OpenShell](https://github.com/NVIDIA/OpenShell) sandboxes — manage them with
`kubectl apply` instead of talking to the gateway directly.

> **Status:** early development. Milestone 1 implements the `OpenShellSandbox`
> resource (create / get / delete with finalizer cleanup and status mirroring).

## What it does

The operator is a thin front-end over the OpenShell gateway's gRPC API. You
declare desired state as custom resources; the operator reconciles them into
gateway calls and mirrors gateway state back into the resource `.status`. It
does not reimplement the gateway.

```yaml
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellSandbox
metadata:
  name: my-sandbox
spec:
  image: ghcr.io/nvidia/openshell-community/sandboxes/python:latest
  environment:
    LOG_LEVEL: debug
  providers:
    - openai
  gpu: false
```

```console
$ kubectl get oss
NAME         PHASE   SANDBOX    AGE
my-sandbox   Ready   3f2b...    30s
```

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

# Regenerate the CRD manifest from the Rust types
cargo run --bin crdgen > deploy/crds/openshellsandbox.yaml

# Run against the current kubecontext. Expects a gateway at
# $OPENSHELL_GATEWAY_ENDPOINT (default http://127.0.0.1:8080).
cargo run --bin openshell-operator
```

`openshell-sdk` and `openshell-core` are consumed as git dependencies pinned to
an exact revision of NVIDIA/OpenShell `main`.

## License

Apache-2.0.
