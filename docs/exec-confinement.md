# Confining `kubectl exec` into sandbox pods

## The gap

An OpenShell sandbox's workload runs in an agent container that is **privileged**
— in the default "combined" topology it runs as root (uid 0) with the caps the
supervisor needs (`SYS_ADMIN`, `NET_ADMIN`). The confinement a user cares about
(non-root uid, Landlock filesystem/network policy, dropped caps) is applied by
the *supervisor* to the workload process, per-process, at runtime.

A plain `kubectl exec` does not go through the supervisor. The kubelet execs the
command directly in the container, so the shell lands as **root, outside the
sandbox** — it inherits the container's privileges, not the workload's
confinement. Gateway RBAC governs exec *through the gateway API* (see
[operator-auth.md](operator-auth.md)); it does not govern a direct `kubectl
exec` against the pod, which is a pure Kubernetes operation.

This is opt-in and off by default (`webhook.execConfinement.enabled`).

## The mechanism

Two admission webhooks, served by the operator process itself:

- **Mutating**, on `pods/exec` (CONNECT). The apiserver runs mutating admission
  on the `PodExecOptions` — including its `command` — *before* it dials the
  kubelet. The webhook rewrites the command for the sandboxed agent container,
  prepending the supervisor entrypoint:

  ```
  id            →   /opt/openshell/bin/openshell-sandbox --mode=process -- id
  ```

  `--mode=process` makes the supervisor re-derive the sandbox's policy from the
  live gateway (via the `OPENSHELL_SANDBOX_ID` / endpoint env already on the
  container) and re-enter the sandbox domain, then exec the user's command. The
  shell drops to the sandbox user under Landlock. Streaming/PTY is unaffected —
  only the argv is rewritten.

- **Validating**, on `pods/attach` (CONNECT) and `pods/ephemeralcontainers`
  (UPDATE). `attach` connects to the root supervisor's stdio, and a `kubectl
  debug` ephemeral container runs unwrapped as root — both sidestep the exec
  rewrite. The webhook denies them outright on sandbox pods. Without this, the
  exec confinement is theatre.

The confinement itself is entirely the upstream supervisor's; the operator only
translates a Kubernetes mechanism (admission) into invoking it — consistent with
its thin-front-end role.

## How a sandbox pod is recognised

The exec admission request carries `PodExecOptions` and the pod's
name/namespace, but **not** the pod's labels — so label-based `objectSelector` /
`matchConditions` can't classify it. The webhook therefore GETs the pod and
classifies by **ownerReference to `agents.x-k8s.io/Sandbox`**. That is the
authority: the gateway's Kubernetes driver always creates an `agents.x-k8s.io`
`Sandbox`, whose controller owns the pod — so this catches every sandbox,
including ones created directly against the gateway rather than through the
operator's CRD. Labels are user-controlled and would miss those.

Within a sandbox pod, only the container carrying the `OPENSHELL_SANDBOX_ID` env
(the agent) is wrapped; the target container is resolved the way the API server
does it (requested container, else the `kubectl.kubernetes.io/default-container`
annotation, else the first container). Exec into any *other* container of a
sandbox pod is denied — it shares the pod's namespaces and would be a bypass.

The decision is deliberately biased safe: wrongly wrapping a non-sandbox exec
merely breaks that one exec; wrongly *not* wrapping a sandbox exec is a root
escape. So every ambiguous branch — unresolved container, unfetchable pod —
denies rather than passing through.

## Scope and failure mode

`failurePolicy: Fail` (fail-closed) is the default and the point: if the
operator is unreachable, exec/attach into a confined namespace is **denied**,
never silently downgraded to an unconfined root shell. `Ignore` is available for
those who want fail-open, but it reopens root exec exactly when an attacker can
induce webhook downtime.

The blast radius is bounded by a `namespaceSelector`: the webhooks fire **only**
in namespaces labelled `openshell.lenshq.io/exec-confinement: enabled` (and never
in `kube-system` / `kube-node-lease`, belt-and-braces). An unlabelled cluster
sees zero webhook calls. Crucially, the webhooks match only `pods/exec`,
`pods/attach`, and `pods/ephemeralcontainers` — **never pod creation** — so a
webhook outage can never wedge scheduling, node drains, or workload creation. The
worst case is "can't `kubectl exec` into a confined namespace until the operator
is back," an interactive operation that humans retry.

Because exec now re-derives policy from the gateway, exec into a sandbox depends
on gateway availability — gateway down means exec fails (consistent with
fail-closed).

Serve the webhook on ≥1 ready replica; with leader election on, the webhook runs
on every replica (leader and standbys alike, outside the leader gate), so a
rolling update or lease flap doesn't drop the listener.

## Serving certificate

The operator self-manages its serving cert — no cert-manager, no chart-baked
cert. On startup it generates a self-signed CA + leaf for the webhook Service
DNS, persists them in a Secret (`<release>-webhook-tls`, adopting a peer
replica's Secret if one raced ahead), and injects the CA into both webhook
configs' `caBundle` via a strategic-merge patch keyed on the webhook name. The
cert is long-lived (no rotation in v1, matching the bundled OIDC token); to
rotate, delete the Secret and restart. The chart never renders `caBundle`, so
`helm upgrade` leaves the injected value intact; under Argo/Flux, add `caBundle`
to `ignoreDifferences` so it isn't flagged as drift.

## Enabling it

```yaml
webhook:
  execConfinement:
    enabled: true
```

Then label the namespaces whose sandboxes should be confined:

```
kubectl label namespace <ns> openshell.lenshq.io/exec-confinement=enabled
```

The `wrapper` argv defaults to the supervisor entrypoint baked into the sandbox
image (`/opt/openshell/bin/openshell-sandbox --mode=process --`); override it
only if that upstream image path changes. `kubectl cp` (exec + tar) keeps
working but writes as the sandbox user under Landlock — copies outside allowed
paths fail, which is the intended confinement, not a bug. `port-forward` reaches
only pod-local sockets and is left untouched.
