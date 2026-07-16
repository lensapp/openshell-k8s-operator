// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Custom resource definitions for the operator's API group.
//!
//! These types are pure schema: they derive the CRD and (de)serialize against
//! the Kubernetes API. Mapping to and from the gateway's SDK types lives in the
//! controller, keeping this module free of the gateway dependency.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Desired state for a single OpenShell sandbox.
///
/// Milestone 1 wires the fields the SDK's curated `SandboxSpec` supports.
/// `policyRef` is accepted and stored now but not yet applied (milestone 3);
/// gateway selection, entrypoint, and TTL/cleanup arrive in later milestones.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "openshell.lenshq.io",
    version = "v1alpha1",
    kind = "OpenShellSandbox",
    namespaced,
    status = "OpenShellSandboxStatus",
    shortname = "oss",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Sandbox","type":"string","jsonPath":".status.sandboxId"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct OpenShellSandboxSpec {
    /// Container image the sandbox runs. Empty defers to the gateway default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// Environment variables injected into the sandbox runtime.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,

    /// Provider names to attach. Each must already exist on the gateway.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,

    /// Request a GPU for the sandbox.
    #[serde(default)]
    pub gpu: bool,

    /// Name of a `Policy` to apply. Reserved for milestone 3 — stored, not yet wired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_ref: Option<String>,
}

/// Observed state mirrored from the gateway.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenShellSandboxStatus {
    /// Coarse lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,

    /// Gateway-assigned sandbox identifier, once created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_id: Option<String>,

    /// `.metadata.generation` last reconciled, for GitOps health checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

/// Coarse sandbox lifecycle phase surfaced in `.status.phase`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub enum Phase {
    /// Gateway is provisioning the sandbox.
    Provisioning,
    /// Sandbox is ready to use.
    Ready,
    /// Sandbox is in an error state on the gateway.
    Error,
    /// Sandbox is being torn down.
    Deleting,
}

/// Desired state for an OpenShell credential provider.
///
/// Credential values are never stored on this resource — they live in a Secret
/// referenced by [`ProviderSpec::credentials_secret_ref`] and are resolved at
/// reconcile time.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "openshell.lenshq.io",
    version = "v1alpha1",
    kind = "Provider",
    namespaced,
    status = "ProviderStatus",
    shortname = "osp",
    printcolumn = r#"{"name":"Type","type":"string","jsonPath":".spec.type"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSpec {
    /// Canonical provider type slug (e.g. `claude`, `gitlab`). Immutable —
    /// enforced by the validating webhook (milestone 4).
    #[serde(rename = "type")]
    pub provider_type: String,

    /// Secret (in this Provider's namespace) holding credential values.
    pub credentials_secret_ref: SecretRef,

    /// Non-secret provider configuration passed through to the gateway.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
}

/// Reference to a Secret providing credential values.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    /// Name of the Secret. Always resolved in the referencing resource's own
    /// namespace — there is no cross-namespace reference.
    pub name: String,

    /// Subset of Secret keys to use as credentials. Empty uses every key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

/// Observed state mirrored from the gateway for a `Provider`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStatus {
    /// Coarse lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ProviderPhase>,

    /// `.metadata.generation` last reconciled, for GitOps health checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    /// Hash of the last successfully synced credentials + config. Surfaces
    /// Secret rotation and lets operators see when a resync last changed state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_hash: Option<String>,
}

/// Coarse provider lifecycle phase surfaced in `.status.phase`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub enum ProviderPhase {
    /// Credentials resolved and synced to the gateway.
    Ready,
    /// Secret missing, not entitled, or the gateway rejected the sync.
    Error,
}
