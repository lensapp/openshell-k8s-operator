# Security Policy

The OpenShell operator is a security-sensitive control-plane component. It reads
Kubernetes `Secret`s, authenticates to the OpenShell gateway as an administrator,
and translates custom resources into gateway API calls. Credential handling, the
Secret-to-gateway path, the gateway authentication, and the operator's RBAC are
all security boundaries.

## Reporting Vulnerabilities

Please do not open a public issue for suspected vulnerabilities.

Report security issues by emailing:

```text
security@lenshq.io
```

Include as much detail as possible:

- Affected component or file path, if known
- Impact and the security boundary you believe is crossed
- Reproduction steps or proof of concept
- Environment details: Kubernetes version, chart values, gateway version, and
  whether the bundled or a bring-your-own gateway is in use
- Whether the issue involves credential exposure, cross-namespace Secret access,
  gateway-authentication bypass, or RBAC escalation

We will acknowledge reports as quickly as practical and coordinate remediation
before public disclosure.

## Security Scope

Security-sensitive areas include:

- **Credential handling** — resolving provider credentials from a `Secret` and
  syncing them to the gateway. A real secret value must never be written onto a
  custom resource or into `.status`; for a gateway-mintable credential the seed
  material is routed to the gateway's refresh configuration, not stored as a
  static provider credential.
- **The entitlement check** — a `Secret` is referenceable as provider
  credentials only in the referencing resource's own namespace *and* only when it
  opts in with the `openshell.lenshq.io/allow-provider-ref: "true"` annotation.
  Any path that resolves a Secret without both conditions is a boundary break.
- **Gateway authentication** — the OIDC bearer the operator presents (minted by
  the bundled issuer), the TLS trust to the gateway, and the administrative scope
  the operator operates with.
- **RBAC blast radius** — the operator's `ClusterRole` (it reads `Secret`s to
  resolve provider credentials) and the leader-election lease permissions.
- **Policy validation** — policy documents are validated by the gateway's parser
  before they are applied to a sandbox; the operator trusts the gateway as the
  validation authority.
- **CRD surface** — untrusted or malformed custom resources must not induce the
  operator to leak credentials or act outside its intended scope.

## Security Boundaries

The operator is a thin front-end over the gateway: it converts declarative custom
resources into gateway calls and mirrors gateway state back into `.status`. The
intended boundary is that Kubernetes `Secret` values reach only the gateway (over
an authenticated, TLS-protected channel) and never a custom resource or its
status; that a `Secret` can be referenced only from within its own namespace and
only with an explicit entitlement annotation; and that the operator authenticates
to exactly the gateway it is configured for.

When in doubt, treat any change touching credential resolution, the entitlement
check, the Secret-to-gateway path, gateway authentication, or the operator's RBAC
as security-sensitive.
