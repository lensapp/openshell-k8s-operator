#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# End-to-end checks for the control-plane CRDs against a live operator+gateway.
#
# Deliberately cluster-agnostic: CI points it at a kind cluster, but it runs
# unchanged against colima or any cluster with the chart installed. It only
# touches resources it creates (all named `e2e-*`), so it is safe to run against
# a development cluster with other resources in it.
#
# `OpenShellSandbox` is out of scope: it needs the agents.x-k8s.io sandbox
# controller and real sandbox pods. Everything here exercises the operator's
# translation into gateway calls, which is what the reconcilers actually own.
#
#   NAMESPACE=openshell-system test/e2e/run.sh

set -euo pipefail

NAMESPACE="${NAMESPACE:-openshell-system}"
# Generous: the gateway is reached over the network and reconciles are requeued
# on a 15s error cadence, so a single retry must fit inside this.
TIMEOUT="${TIMEOUT:-90s}"
# How long a status field may take to reach an expected value, in seconds.
POLL_SECONDS="${POLL_SECONDS:-90}"

PASSED=0

log()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
ok()   { printf '  \033[32mok\033[0m %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*" >&2; return 1; }

# Delete every resource this script creates. Providers go first: they are what
# holds the profile and workspace finalizers open, so removing them lets the
# guarded deletes complete instead of blocking until the timeout.
#
# The finalizer-guarded kinds are waited on: with the providers already gone
# nothing holds their finalizers, and leaving them mid-deletion would make an
# immediate re-run fail on "object is being deleted".
cleanup() {
  set +e
  kubectl delete openshellprovider -n "$NAMESPACE" -l e2e=true --wait=true --timeout=60s >/dev/null 2>&1
  kubectl delete openshellproviderprofile -l e2e=true --wait=true --timeout=60s >/dev/null 2>&1
  kubectl delete openshellworkspace -l e2e=true --wait=true --timeout=60s >/dev/null 2>&1
  kubectl delete openshellpolicy -n "$NAMESPACE" -l e2e=true --wait=false >/dev/null 2>&1
  kubectl delete secret -n "$NAMESPACE" -l e2e=true --wait=false >/dev/null 2>&1
  set -e
}

# On failure, dump what a human would ask for first.
diagnose() {
  printf '\n\033[31m=== e2e failed; dumping diagnostics ===\033[0m\n' >&2
  kubectl get openshellworkspace,openshellproviderprofile -o wide >&2 2>&1 || true
  kubectl get openshellprovider,openshellpolicy -n "$NAMESPACE" -o wide >&2 2>&1 || true
  kubectl get pods -n "$NAMESPACE" >&2 2>&1 || true
  printf '\n--- operator logs ---\n' >&2
  kubectl logs -n "$NAMESPACE" -l app.kubernetes.io/name=openshell-operator --tail=80 >&2 2>&1 || true
  printf '\n--- gateway logs ---\n' >&2
  kubectl logs -n "$NAMESPACE" statefulset/openshell-gateway --tail=40 >&2 2>&1 || true
  # Cleanup is left to the EXIT trap, which is its single owner.
}
trap diagnose ERR
trap cleanup EXIT

# Poll until `kubectl get -o jsonpath=<path>` on a resource equals <expected>.
# Used for status fields that are not conditions (phase, reason, resourceVersion).
await() { # await <resource> <jsonpath> <expected> [-n namespace]
  local resource="$1" path="$2" expected="$3"; shift 3
  local deadline=$((SECONDS + POLL_SECONDS)) actual=''
  while [ "$SECONDS" -lt "$deadline" ]; do
    actual="$(kubectl get "$resource" "$@" -o jsonpath="$path" 2>/dev/null || true)"
    [ "$actual" = "$expected" ] && return 0
    sleep 2
  done
  fail "$resource $path: expected '$expected', got '$actual'"
}

# Read one status field.
field() { # field <resource> <jsonpath> [-n namespace]
  local resource="$1" path="$2"; shift 2
  kubectl get "$resource" "$@" -o jsonpath="$path" 2>/dev/null || true
}

ready_reason='{.status.conditions[?(@.type=="Ready")].reason}'

log "Preflight: operator and gateway are up in $NAMESPACE"
kubectl wait --for=condition=Available --timeout="$TIMEOUT" \
  -n "$NAMESPACE" deployment -l app.kubernetes.io/name=openshell-operator >/dev/null
ok "operator deployment available"
kubectl rollout status --timeout="$TIMEOUT" -n "$NAMESPACE" statefulset/openshell-gateway >/dev/null
ok "gateway statefulset rolled out"

# --------------------------------------------------------------------------
log "OpenShellPolicy: validated by the gateway's parser"
# --------------------------------------------------------------------------
kubectl apply -f - >/dev/null <<EOF
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellPolicy
metadata: { name: e2e-policy, namespace: $NAMESPACE, labels: { e2e: "true" } }
spec:
  version: 1
  filesystem: { includeWorkdir: true, readOnly: ["/etc"] }
EOF
kubectl wait --for=condition=Ready --timeout="$TIMEOUT" \
  -n "$NAMESPACE" openshellpolicy/e2e-policy >/dev/null
ok "valid policy reports Ready"

# An unknown key inside a network rule: the CRD passes it through opaquely and
# the gateway's parser rejects it, which is the whole point of that design.
kubectl apply -f - >/dev/null <<EOF
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellPolicy
metadata: { name: e2e-policy-bad, namespace: $NAMESPACE, labels: { e2e: "true" } }
spec:
  version: 1
  networkPolicies:
    bad: { nonsense: true }
EOF
await openshellpolicy/e2e-policy-bad "$ready_reason" PolicyInvalid -n "$NAMESPACE"
ok "invalid policy reports Ready=False/PolicyInvalid"

# --------------------------------------------------------------------------
log "OpenShellProviderProfile: import, update, validate, reference guard"
# --------------------------------------------------------------------------
kubectl apply -f - >/dev/null <<EOF
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellProviderProfile
metadata: { name: e2e-profile, labels: { e2e: "true" } }
spec:
  displayName: E2E Profile
  category: inference
  inferenceCapable: true
  credentials:
    - name: api_key
      env_vars: ["E2E_API_KEY"]
      required: true
  endpoints:
    - host: api.e2e.example
      port: 443
EOF
kubectl wait --for=condition=Ready --timeout="$TIMEOUT" openshellproviderprofile/e2e-profile >/dev/null
ok "profile imported and reports Ready"

imported_rv="$(field openshellproviderprofile/e2e-profile '{.status.resourceVersion}')"
[ -n "$imported_rv" ] || fail "profile status.resourceVersion is empty after import"
ok "gateway resource version mirrored to status ($imported_rv)"

# Editing the profile must take the gateway's update path, which bumps the
# stored resource version rather than re-importing. Wait on the generation the
# patch actually produced, rather than assuming this is the object's first edit.
kubectl patch openshellproviderprofile e2e-profile --type merge \
  -p '{"spec":{"displayName":"E2E Profile v2"}}' >/dev/null
patched_generation="$(field openshellproviderprofile/e2e-profile '{.metadata.generation}')"
await openshellproviderprofile/e2e-profile '{.status.observedGeneration}' "$patched_generation"
updated_rv="$(field openshellproviderprofile/e2e-profile '{.status.resourceVersion}')"
[ -n "$updated_rv" ] || fail "profile status.resourceVersion is empty after update"
[ "$updated_rv" != "$imported_rv" ] || fail "resource version unchanged after update ($updated_rv)"
ok "update bumped the gateway resource version ($imported_rv -> $updated_rv)"

kubectl apply -f - >/dev/null <<EOF
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellProviderProfile
metadata: { name: e2e-profile-bad, labels: { e2e: "true" } }
spec:
  displayName: Bad
  category: not-a-real-category
EOF
await openshellproviderprofile/e2e-profile-bad "$ready_reason" ProfileInvalid
ok "unsupported category reports Ready=False/ProfileInvalid"
kubectl delete openshellproviderprofile e2e-profile-bad --wait=false >/dev/null

# --------------------------------------------------------------------------
log "OpenShellProvider: credentials, entitlement, immutability"
# --------------------------------------------------------------------------
kubectl apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Secret
metadata:
  name: e2e-creds
  namespace: $NAMESPACE
  labels: { e2e: "true" }
  annotations: { openshell.lenshq.io/allow-provider-ref: "true" }
stringData: { E2E_API_KEY: sk-e2e-secret }
---
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellProvider
metadata: { name: e2e-provider, namespace: $NAMESPACE, labels: { e2e: "true" } }
spec:
  type: e2e-profile
  credentialsSecretRef: { name: e2e-creds }
EOF
kubectl wait --for=condition=Ready --timeout="$TIMEOUT" \
  -n "$NAMESPACE" openshellprovider/e2e-provider >/dev/null
ok "provider synced against the custom profile type"
await openshellprovider/e2e-provider '{.status.credentialMode}' Copied -n "$NAMESPACE"
ok "credential mode reported as Copied"

# A Secret without the entitlement annotation must be refused: that annotation
# is what stops any Secret in the namespace being exfiltrated to the gateway.
kubectl apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Secret
metadata: { name: e2e-creds-unentitled, namespace: $NAMESPACE, labels: { e2e: "true" } }
stringData: { E2E_API_KEY: sk-nope }
---
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellProvider
metadata: { name: e2e-provider-unentitled, namespace: $NAMESPACE, labels: { e2e: "true" } }
spec:
  type: e2e-profile
  credentialsSecretRef: { name: e2e-creds-unentitled }
EOF
await openshellprovider/e2e-provider-unentitled "$ready_reason" SecretNotEntitled -n "$NAMESPACE"
ok "unentitled Secret refused with SecretNotEntitled"

# CEL rules on the CRD: identity fields are rejected at admission, so these
# never reach a reconciler. The rejection must come from the CEL rule — any
# other patch failure (a conflict, an unreachable API server) would otherwise
# read as a pass.
expect_immutable() { # expect_immutable <field> <patch>
  local name="$1" patch="$2" output
  if output="$(kubectl patch openshellprovider e2e-provider -n "$NAMESPACE" \
                 --type merge -p "$patch" 2>&1)"; then
    fail "spec.$name was mutable; the CEL immutability rule did not fire"
  fi
  case "$output" in
    *"spec.$name is immutable"*) ok "spec.$name rejected at admission (CEL)" ;;
    *) fail "spec.$name patch failed, but not with the CEL rule's message: $output" ;;
  esac
}
expect_immutable type '{"spec":{"type":"something-else"}}'
expect_immutable workspace '{"spec":{"workspace":"other"}}'

# --------------------------------------------------------------------------
log "Finalizer guards: in-use resources refuse deletion"
# --------------------------------------------------------------------------
# e2e-provider still selects e2e-profile, so the profile must refuse to go.
kubectl delete openshellproviderprofile e2e-profile --wait=false >/dev/null
await openshellproviderprofile/e2e-profile "$ready_reason" ProfileInUse
ok "in-use profile refuses deletion with ProfileInUse"

kubectl delete openshellprovider e2e-provider e2e-provider-unentitled \
  -n "$NAMESPACE" --wait=true --timeout=60s >/dev/null
deadline=$((SECONDS + POLL_SECONDS))
while kubectl get openshellproviderprofile e2e-profile >/dev/null 2>&1; do
  [ "$SECONDS" -lt "$deadline" ] || fail "profile still present after its providers were removed"
  sleep 2
done
ok "profile deletes once no provider selects it"

# --------------------------------------------------------------------------
log "OpenShellWorkspace: tenancy boundary and non-empty guard"
# --------------------------------------------------------------------------
kubectl apply -f - >/dev/null <<EOF
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellWorkspace
metadata: { name: e2ews, labels: { e2e: "true" } }
spec:
  labels: { owner: e2e }
EOF
kubectl wait --for=condition=Ready --timeout="$TIMEOUT" openshellworkspace/e2ews >/dev/null
ok "workspace created and reports Ready"
await openshellworkspace/e2ews '{.status.phase}' Active
ok "gateway phase mirrored as Active"

# A provider joining the workspace must block its deletion: the gateway marks a
# workspace terminating before checking for blockers, with no undelete.
#
# Uses the gateway's built-in `claude-code` profile rather than e2e-profile,
# which the reference-guard check above has already deleted. If a gateway bump
# ever renames that built-in, this is the line that fails.
kubectl apply -f - >/dev/null <<EOF
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellProvider
metadata: { name: e2e-provider-ws, namespace: $NAMESPACE, labels: { e2e: "true" } }
spec:
  type: claude-code
  workspace: e2ews
  credentialsSecretRef: { name: e2e-creds }
EOF
kubectl wait --for=condition=Ready --timeout="$TIMEOUT" \
  -n "$NAMESPACE" openshellprovider/e2e-provider-ws >/dev/null
ok "provider joined the workspace"

kubectl delete openshellworkspace e2ews --wait=false >/dev/null
await openshellworkspace/e2ews "$ready_reason" WorkspaceNotEmpty
ok "non-empty workspace refuses deletion with WorkspaceNotEmpty"

kubectl delete openshellprovider e2e-provider-ws -n "$NAMESPACE" --wait=true --timeout=60s >/dev/null
deadline=$((SECONDS + POLL_SECONDS))
while kubectl get openshellworkspace e2ews >/dev/null 2>&1; do
  [ "$SECONDS" -lt "$deadline" ] || fail "workspace still present after it was emptied"
  sleep 2
done
ok "workspace deletes once emptied"

printf '\n\033[32mAll %d e2e checks passed.\033[0m\n' "$PASSED"
