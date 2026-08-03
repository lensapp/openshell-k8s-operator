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
# The `OpenShellSandbox` section needs the agents.x-k8s.io sandbox controller
# and pulls a real sandbox image, so it runs only when that CRD is present. Set
# SANDBOX_E2E=1 to make its absence a failure instead of a skip, or 0 to skip
# the section outright.
#
#   NAMESPACE=openshell-system test/e2e/run.sh
#
# A bring-your-own gateway runs in its own namespace; name it so the preflight
# and the diagnostics look in the right place:
#
#   NAMESPACE=openshell-system GATEWAY_NAMESPACE=openshell-gateway test/e2e/run.sh

set -euo pipefail

NAMESPACE="${NAMESPACE:-openshell-system}"
# Where the gateway runs. Defaults to the operator's namespace, which is where
# the bundled install puts it; a bring-your-own gateway lives wherever its own
# release does, so the preflight and the diagnostics take it as a knob.
GATEWAY_NAMESPACE="${GATEWAY_NAMESPACE:-$NAMESPACE}"
# Generous: the gateway is reached over the network and reconciles are requeued
# on a 15s error cadence, so a single retry must fit inside this.
TIMEOUT="${TIMEOUT:-90s}"
# How long a status field may take to reach an expected value, in seconds.
POLL_SECONDS="${POLL_SECONDS:-90}"
# Sandboxes are slower than the control-plane kinds by a different order: a real
# pod is scheduled and the sandbox image (~1.4 GB) is pulled on first use.
SANDBOX_TIMEOUT="${SANDBOX_TIMEOUT:-600s}"
SANDBOX_POLL_SECONDS="${SANDBOX_POLL_SECONDS:-600}"
# auto — run the sandbox section when the agents.x-k8s.io CRD is there.
SANDBOX_E2E="${SANDBOX_E2E:-auto}"

PASSED=0

log()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
ok()   { printf '  \033[32mok\033[0m %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*" >&2; return 1; }
skip() { printf '  \033[33mskip\033[0m %s\n' "$*"; }

# Delete every resource this script creates. Providers go first: they are what
# holds the profile and workspace finalizers open, so removing them lets the
# guarded deletes complete instead of blocking until the timeout.
#
# The finalizer-guarded kinds are waited on: with the providers already gone
# nothing holds their finalizers, and leaving them mid-deletion would make an
# immediate re-run fail on "object is being deleted".
cleanup() {
  # Best-effort deletes: a failure here is not a finding.
  set +e
  # Sandboxes first: their finalizer deletes the gateway sandbox, and the PVCs
  # they provisioned outlive them by design (volumeRetention: Retain). Those
  # PVCs carry the operator's own sandbox label, not the script's `e2e` one.
  kubectl delete openshellsandbox -n "$NAMESPACE" -l e2e=true --wait=true --timeout=120s >/dev/null 2>&1
  kubectl delete pvc -n "$NAMESPACE" -l openshell.lenshq.io/sandbox=e2e-sandbox --wait=false >/dev/null 2>&1
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
  kubectl get openshellsandbox,pvc -n "$NAMESPACE" -o wide >&2 2>&1 || true
  kubectl get pods -n "$NAMESPACE" >&2 2>&1 || true
  printf '\n--- operator logs ---\n' >&2
  kubectl logs -n "$NAMESPACE" -l app.kubernetes.io/name=openshell-operator --tail=80 >&2 2>&1 || true
  printf '\n--- gateway logs ---\n' >&2
  kubectl logs -n "$GATEWAY_NAMESPACE" statefulset/openshell-gateway --tail=40 >&2 2>&1 || true
  # Cleanup is `finish`'s job, and runs after this returns.
}
# One trap for the whole exit path: dump diagnostics when the script is on its
# way out with a failure, then clean up either way. Keyed on the exit status
# rather than on ERR, because plenty of commands here are *expected* to fail —
# the patches the CEL rules must reject, the polls that read a resource before
# it exists — and an ERR trap dumps a scary diagnostic for every one of them.
finish() {
  local status=$?
  [ "$status" -eq 0 ] || diagnose
  cleanup
}
trap finish EXIT

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

# Poll until a status field holds something other than <previous> — for values
# whose new content is the operator's to choose (a fresh sandbox id, a rehashed
# fingerprint). An empty read does not count: a field cleared mid-recreate is
# not yet the new value.
away() { # away <resource> <jsonpath> <previous> [-n namespace]
  local resource="$1" path="$2" previous="$3"; shift 3
  local deadline=$((SECONDS + POLL_SECONDS)) actual=''
  while [ "$SECONDS" -lt "$deadline" ]; do
    actual="$(kubectl get "$resource" "$@" -o jsonpath="$path" 2>/dev/null || true)"
    [ -n "$actual" ] && [ "$actual" != "$previous" ] && return 0
    sleep 2
  done
  fail "$resource $path: still '$previous' after ${POLL_SECONDS}s"
}

# Poll until the resource has emitted an event with <reason>. Needs kubectl
# 1.26 or newer for `kubectl events --for`; an older one just never matches.
event() { # event <kind/name> <reason>
  local resource="$1" reason="$2"
  local deadline=$((SECONDS + POLL_SECONDS))
  while [ "$SECONDS" -lt "$deadline" ]; do
    kubectl events -n "$NAMESPACE" --for "$resource" -o jsonpath='{.items[*].reason}' 2>/dev/null \
      | grep -qw "$reason" && return 0
    sleep 2
  done
  fail "$resource emitted no $reason event"
}

# Read one status field.
field() { # field <resource> <jsonpath> [-n namespace]
  local resource="$1" path="$2"; shift 2
  kubectl get "$resource" "$@" -o jsonpath="$path" 2>/dev/null || true
}

ready_reason='{.status.conditions[?(@.type=="Ready")].reason}'

log "Preflight: operator in $NAMESPACE, gateway in $GATEWAY_NAMESPACE"
kubectl wait --for=condition=Available --timeout="$TIMEOUT" \
  -n "$NAMESPACE" deployment -l app.kubernetes.io/name=openshell-operator >/dev/null
ok "operator deployment available"
kubectl rollout status --timeout="$TIMEOUT" -n "$GATEWAY_NAMESPACE" statefulset/openshell-gateway >/dev/null
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

# --------------------------------------------------------------------------
log "OpenShellSandbox: lifecycle, in-place policy update, recreate, volumes"
# --------------------------------------------------------------------------
# The only section that drives a real sandbox pod end to end. It owns its own
# policy: the checks below mutate it, and the sections above assert on theirs.
#
# `spec.image` is deliberately unset so the gateway picks its own default image
# — the test then has no image reference of its own to keep current.
sandbox_checks() {
  # Every poll in this section waits on a pod, not on a gRPC round trip, so it
  # gets the sandbox budget. `local` is what raises it for the `await` / `away`
  # calls below: they read POLL_SECONDS while this function is on the stack.
  local POLL_SECONDS="$SANDBOX_POLL_SECONDS"

  kubectl apply -f - >/dev/null <<EOF
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellPolicy
metadata: { name: e2e-sandbox-policy, namespace: $NAMESPACE, labels: { e2e: "true" } }
spec:
  version: 1
  filesystem: { includeWorkdir: true }
---
apiVersion: openshell.lenshq.io/v1alpha1
kind: OpenShellSandbox
metadata: { name: e2e-sandbox, namespace: $NAMESPACE, labels: { e2e: "true" } }
spec:
  policyRef: e2e-sandbox-policy
  volumes:
    - name: work
      mountPath: /data
      claim:
        accessModes: ["ReadWriteOnce"]
        resources: { requests: { storage: 1Gi } }
EOF
  kubectl wait --for=condition=Ready --timeout="$SANDBOX_TIMEOUT" \
    -n "$NAMESPACE" openshellsandbox/e2e-sandbox >/dev/null
  ok "sandbox reports Ready"
  await openshellsandbox/e2e-sandbox '{.status.phase}' Ready -n "$NAMESPACE"
  ok "gateway phase mirrored as Ready"

  local sandbox_id policy_hash spec_hash pvc_uid
  sandbox_id="$(field openshellsandbox/e2e-sandbox '{.status.sandboxId}' -n "$NAMESPACE")"
  [ -n "$sandbox_id" ] || fail "sandbox status.sandboxId is empty while Ready"
  ok "gateway sandbox id mirrored to status ($sandbox_id)"

  # The claim is provisioned by the operator, not the gateway — that ownership
  # is what lets it outlive the recreate below.
  kubectl wait --for=jsonpath='{.status.phase}'=Bound --timeout="$TIMEOUT" \
    -n "$NAMESPACE" pvc/e2e-sandbox-work >/dev/null
  pvc_uid="$(field pvc/e2e-sandbox-work '{.metadata.uid}' -n "$NAMESPACE")"
  ok "operator-provisioned PVC bound"

  # A mutable policy section (networkPolicies) goes to the live sandbox through
  # UpdateConfig. The sandbox id must survive: recreating here would discard the
  # running workload for a change the gateway accepts in place.
  policy_hash="$(field openshellsandbox/e2e-sandbox '{.status.appliedPolicyHash}' -n "$NAMESPACE")"
  # An empty baseline would turn the `away` below into "wait for any value",
  # which the very first hash write satisfies without converging anything.
  [ -n "$policy_hash" ] || fail "sandbox status.appliedPolicyHash is empty while Ready"
  kubectl patch openshellpolicy e2e-sandbox-policy -n "$NAMESPACE" --type merge -p \
    '{"spec":{"networkPolicies":{"e2e":{"endpoints":[{"host":"api.e2e.example","port":443}]}}}}' >/dev/null
  away openshellsandbox/e2e-sandbox '{.status.appliedPolicyHash}' "$policy_hash" -n "$NAMESPACE"
  ok "policy edit converged in place (appliedPolicyHash moved)"
  event openshellsandbox/e2e-sandbox PolicyUpdated
  ok "PolicyUpdated event emitted"
  [ "$(field openshellsandbox/e2e-sandbox '{.status.sandboxId}' -n "$NAMESPACE")" = "$sandbox_id" ] \
    || fail "a mutable policy change recreated the sandbox"
  ok "gateway sandbox kept its id"

  # An immutable field (logLevel) can only converge by recreation. Chosen over
  # `image` so the recreate does not pull a second multi-gigabyte image.
  spec_hash="$(field openshellsandbox/e2e-sandbox '{.status.appliedSpecHash}' -n "$NAMESPACE")"
  [ -n "$spec_hash" ] || fail "sandbox status.appliedSpecHash is empty while Ready"
  kubectl patch openshellsandbox e2e-sandbox -n "$NAMESPACE" --type merge -p \
    '{"spec":{"logLevel":"debug"}}' >/dev/null
  away openshellsandbox/e2e-sandbox '{.status.sandboxId}' "$sandbox_id" -n "$NAMESPACE"
  ok "immutable field edit recreated the gateway sandbox"
  event openshellsandbox/e2e-sandbox Recreating
  ok "Recreating event emitted"
  away openshellsandbox/e2e-sandbox '{.status.appliedSpecHash}' "$spec_hash" -n "$NAMESPACE"
  ok "appliedSpecHash tracks the new immutable fingerprint"
  kubectl wait --for=condition=Ready --timeout="$SANDBOX_TIMEOUT" \
    -n "$NAMESPACE" openshellsandbox/e2e-sandbox >/dev/null
  # The condition reports reconcile health; the phase reports the gateway's own
  # lifecycle. Check both, or a recreate that never produced a running sandbox
  # would pass on the condition alone.
  await openshellsandbox/e2e-sandbox '{.status.phase}' Ready -n "$NAMESPACE"
  ok "recreated sandbox reports Ready and reaches phase Ready"

  # The whole point of operator-owned volumes: same claim object, not a new one
  # that happens to carry the same name.
  [ "$(field pvc/e2e-sandbox-work '{.metadata.uid}' -n "$NAMESPACE")" = "$pvc_uid" ] \
    || fail "the PVC was replaced by the recreate"
  await pvc/e2e-sandbox-work '{.status.phase}' Bound -n "$NAMESPACE"
  ok "operator-owned volume survived the recreate"

  # Deletion runs the finalizer, which deletes the gateway sandbox. The default
  # volumeRetention (Retain) keeps the data behind.
  kubectl delete openshellsandbox e2e-sandbox -n "$NAMESPACE" \
    --wait=true --timeout="$SANDBOX_TIMEOUT" >/dev/null
  ok "sandbox deleted; finalizer cleanup completed"
  kubectl get pvc e2e-sandbox-work -n "$NAMESPACE" >/dev/null 2>&1 \
    || fail "PVC removed despite volumeRetention: Retain"
  ok "PVC retained after deletion"
}

if [ "$SANDBOX_E2E" = "0" ]; then
  skip "SANDBOX_E2E=0"
elif kubectl get crd sandboxes.agents.x-k8s.io >/dev/null 2>&1; then
  sandbox_checks
elif [ "$SANDBOX_E2E" = "1" ]; then
  fail "SANDBOX_E2E=1 but the agents.x-k8s.io Sandbox CRD is absent"
else
  skip "no agents.x-k8s.io Sandbox CRD; install agent-sandbox to run this section"
fi

printf '\n\033[32mAll %d e2e checks passed.\033[0m\n' "$PASSED"
