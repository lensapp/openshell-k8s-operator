# Claude Code sandbox example

A self-contained example modelled on the OpenShell-Community [`base`](https://github.com/NVIDIA/OpenShell-Community/tree/main/sandboxes/base) sandbox. [`claude-code.yaml`](claude-code.yaml) declares five resources that fit together:

- an **`OpenShellProviderProfile`** (cluster-scoped) that imports the `claude-code` provider type, which the gateway does not ship,
- a **Secret** holding the Anthropic credential (opted in with `openshell.lenshq.io/allow-provider-ref: "true"`),
- an **`OpenShellProvider`** (`type: claude-code`) that binds it on the gateway,
- an **`OpenShellPolicy`** mirroring the base sandbox's filesystem / landlock / process posture and its Claude + GitHub network policies,
- an **`OpenShellSandbox`** running the base image that pulls both together via `providers` and `policyRef`.

The Secret ships a placeholder key. Replace it with a real one for an actual Claude round-trip; everything below except that round-trip works with the placeholder.

## Prerequisites

A cluster with the Agent Sandbox CRDs and a running OpenShell gateway + this operator — see the repo [README](../../README.md#install). The manifest targets the `openshell-system` namespace; edit it if yours differs.

## Apply

```bash
kubectl apply -f examples/claude-code/claude-code.yaml
```

## Verify provisioning (the operator's job)

This is pure `kubectl` — it confirms the operator reconciled each resource (`AGE` column omitted):

```console
$ kubectl get ospp claude-code
NAME          CATEGORY   READY
claude-code   agent      True

$ kubectl -n openshell-system get osp anthropic
NAME        TYPE          MODE     READY
anthropic   claude-code   Copied   True

$ kubectl -n openshell-system get ospol base
NAME   READY
base   True

$ kubectl -n openshell-system get oss claude-code
NAME          READY   PHASE   SANDBOX
claude-code   True    Ready   <id>
```

`Profile Ready` means the gateway imported the provider type, `Provider Copied/Ready` means the credential synced to the gateway, `Policy Ready` means the gateway's parser accepted the policy, and `Sandbox Ready` means the gateway created the pod.

## Access the sandbox (the gateway's job)

Running Claude in the sandbox goes through the gateway, not the operator.

Use the **OpenShell CLI**: `openshell sandbox exec -n claude-code -- claude -p "..."`, or `openshell sandbox connect claude-code` for an interactive shell. It runs as the unprivileged `sandbox` user with the policy enforced, and the gateway's egress proxy injects the Anthropic key on outbound requests, so the raw key never lands in the sandbox. It needs a gateway bearer with the user role and a membership in the sandbox's workspace; see [`docs/operator-auth.md`](../../docs/operator-auth.md#human-access-to-the-gateway-roles).

> **Do not use `kubectl exec` as the access path.** Without the [exec-confinement webhook](../../README.md#confined-kubectl-exec) (off by default for a bring-your-own gateway) it lands you as root with no policy applied. With the webhook it is confined, but it still gets no credential injection, so it cannot reach the Anthropic API, and the gateway does not audit it.

## Clean up

```bash
kubectl delete -f examples/claude-code/claude-code.yaml
```
