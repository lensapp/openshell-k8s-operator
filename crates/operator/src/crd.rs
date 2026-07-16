// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Custom resource definitions for the operator's API group.
//!
//! These types are pure schema: they derive the CRD and (de)serialize against
//! the Kubernetes API. Mapping to and from the gateway's SDK types lives in the
//! controller, keeping this module free of the gateway dependency.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use schemars::r#gen::SchemaGenerator;
use schemars::schema::{Schema, SchemaObject};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Desired state for a single OpenShell sandbox.
///
/// The sandbox's policy is applied at creation time and may be given either
/// inline via `policy` or by reference via `policyRef` (naming an
/// `OpenShellPolicy` in the same namespace) — at most one of the two. Gateway
/// selection, entrypoint, and TTL/cleanup arrive in later milestones.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "openshell.lenshq.io",
    version = "v1alpha1",
    kind = "OpenShellSandbox",
    namespaced,
    status = "OpenShellSandboxStatus",
    shortname = "oss",
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
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
    /// Converged in place on a running sandbox: adding or removing a name
    /// attaches or detaches the provider without a recreate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,

    /// Request a GPU for the sandbox.
    #[serde(default)]
    pub gpu: bool,

    /// Inline policy document applied at create. Mutually exclusive with
    /// `policyRef`. Use this for a one-off, self-contained sandbox; use
    /// `policyRef` to share a reusable, pre-validated policy across sandboxes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<OpenShellPolicySpec>,

    /// Name of an `OpenShellPolicy` (same namespace) whose document is applied
    /// at create. Mutually exclusive with `policy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_ref: Option<String>,

    /// Persistent volumes provisioned and owned by this sandbox. Each entry has
    /// the operator create a PVC and mount it into the sandbox. Because the PVC
    /// is anchored to this resource — not the disposable gateway sandbox — its
    /// data survives the delete+recreate the gateway requires to change an
    /// otherwise-immutable field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<SandboxVolume>,

    /// What happens to the provisioned `volumes` when this `OpenShellSandbox` is
    /// deleted. Volumes are always kept across gateway-side recreation; this
    /// setting only governs deletion of the resource itself.
    #[serde(default)]
    pub volume_retention: VolumeRetention,
}

/// A persistent volume the operator provisions, owns, and mounts into a sandbox.
///
/// The operator creates a `PersistentVolumeClaim` named deterministically from
/// the sandbox and `name`, then wires it into the gateway's Kubernetes driver
/// config so the sandbox pod mounts it at `mountPath`. Mounting under `/sandbox`
/// hands OpenShell's workspace persistence to this volume (the gateway then
/// skips its own ephemeral workspace claim); mount elsewhere (e.g. `/data`) to
/// add durable storage alongside the image-seeded workspace.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SandboxVolume {
    /// Volume name, also the stem of the provisioned PVC's name. Must be
    /// unique within the sandbox and a valid PVC name component (the API
    /// server rejects a malformed name at PVC creation).
    pub name: String,

    /// Absolute path the volume is mounted at inside the sandbox container.
    pub mount_path: String,

    /// Mount only a sub-path within the volume rather than its root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,

    /// Mount the volume read-only.
    #[serde(default)]
    pub read_only: bool,

    /// The `PersistentVolumeClaim` spec used to provision the volume, embedded
    /// verbatim from the Kubernetes API — so `resources`, `storageClassName`,
    /// `accessModes`, and `dataSource` (restore from a snapshot or clone) are
    /// all available. `volumeMode: Block` is rejected: the sandbox mounts a
    /// filesystem.
    pub claim: k8s_openapi::api::core::v1::PersistentVolumeClaimSpec,
}

/// Fate of a sandbox's provisioned volumes when the `OpenShellSandbox` is deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub enum VolumeRetention {
    /// Keep the PVCs so their data outlives the resource (default). The
    /// operator never deletes them; cleanup is left to the user.
    #[default]
    Retain,
    /// Delete the provisioned PVCs when the resource is deleted.
    Delete,
}

/// Observed state mirrored from the gateway.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenShellSandboxStatus {
    /// Standard conditions; the `Ready` condition reports reconcile health and,
    /// on failure, a machine-readable `reason` and human `message`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,

    /// Coarse lifecycle phase mirrored from the gateway (distinct from `Ready`,
    /// which reports the operator's own reconcile outcome).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,

    /// Gateway-assigned sandbox identifier, once created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_id: Option<String>,

    /// Hash of the immutable spec fields last applied to the gateway sandbox.
    /// A change here versus the current spec is immutable drift, which the
    /// operator resolves by deleting and recreating the gateway sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_spec_hash: Option<String>,

    /// Hash of the mutable policy fields (`networkPolicies` + `filesystem`) last
    /// applied. A change here is converged in place via the gateway's
    /// `UpdateConfig`, without recreating the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_policy_hash: Option<String>,

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
/// referenced by [`OpenShellProviderSpec::credentials_secret_ref`] and are resolved at
/// reconcile time.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "openshell.lenshq.io",
    version = "v1alpha1",
    kind = "OpenShellProvider",
    namespaced,
    status = "OpenShellProviderStatus",
    shortname = "osp",
    printcolumn = r#"{"name":"Type","type":"string","jsonPath":".spec.type"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct OpenShellProviderSpec {
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

/// Observed state mirrored from the gateway for an `OpenShellProvider`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenShellProviderStatus {
    /// Standard conditions; the `Ready` condition reports whether credentials
    /// resolved and synced to the gateway, with a `reason`/`message` on failure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,

    /// `.metadata.generation` last reconciled, for GitOps health checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    /// Hash of the last successfully synced credentials + config. Surfaces
    /// Secret rotation and lets operators see when a resync last changed state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_hash: Option<String>,
}

/// A reusable sandbox policy document.
///
/// The high-value, stable fields (`filesystem`, `landlock`, `process`) are
/// typed. `networkPolicies` is left opaque (preserve-unknown) because the
/// gateway's L7 endpoint schema is large and fast-moving; validation is
/// delegated wholesale to the gateway's own `openshell-policy` parser at
/// reconcile time rather than mirrored (and inevitably drifting) here.
///
/// An `OpenShellPolicy` holds no gateway state and is not synced on its own — it is a
/// document that `OpenShellSandbox.spec.policyRef` resolves and applies at
/// sandbox creation (the same schema can also be inlined directly under
/// `OpenShellSandbox.spec.policy`). Note that `filesystem`, `landlock`, and
/// `process` are immutable on a running sandbox, so editing an `OpenShellPolicy` only affects
/// sandboxes created afterwards.
#[derive(CustomResource, Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "openshell.lenshq.io",
    version = "v1alpha1",
    kind = "OpenShellPolicy",
    namespaced,
    status = "OpenShellPolicyStatus",
    shortname = "ospol",
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct OpenShellPolicySpec {
    /// Policy schema version understood by the gateway. Defaults to `1`.
    #[serde(default = "default_policy_version")]
    pub version: u32,

    /// Filesystem access policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<FilesystemPolicy>,

    /// Landlock configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landlock: Option<LandlockPolicy>,

    /// Process execution identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessPolicy>,

    /// Network access rules keyed by name (e.g. `claude_code`, `gitlab`). The
    /// value schema is the gateway's `NetworkPolicyRule` and is passed through
    /// verbatim; see the OpenShell policy documentation for the field set.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub network_policies: BTreeMap<String, PreservedValue>,
}

/// An arbitrary JSON value whose schema the API server leaves open
/// (`x-kubernetes-preserve-unknown-fields`).
///
/// Used for `networkPolicies` values: the gateway's L7 network-rule schema is
/// large and evolves independently, so mirroring it in the CRD would only
/// invite drift. The gateway's parser is the single validation authority.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct PreservedValue(pub serde_json::Value);

impl JsonSchema for PreservedValue {
    fn schema_name() -> String {
        "PreservedValue".to_owned()
    }

    // Inline the schema rather than emit a `$ref`, which CRD generation cannot
    // resolve.
    fn is_referenceable() -> bool {
        false
    }

    fn json_schema(_gen: &mut SchemaGenerator) -> Schema {
        let mut schema = SchemaObject::default();
        schema.extensions.insert(
            "x-kubernetes-preserve-unknown-fields".to_owned(),
            true.into(),
        );
        Schema::Object(schema)
    }
}

/// Default policy schema version.
const fn default_policy_version() -> u32 {
    1
}

/// Filesystem access policy. Mirrors the gateway's `FilesystemPolicy`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemPolicy {
    /// Mount the sandbox working directory read-write.
    #[serde(default)]
    pub include_workdir: bool,

    /// Absolute paths mounted read-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_only: Vec<String>,

    /// Absolute paths mounted read-write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_write: Vec<String>,
}

/// Landlock configuration. Mirrors the gateway's `LandlockPolicy`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LandlockPolicy {
    /// Landlock compatibility mode (e.g. `best_effort`, `enforce`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub compatibility: String,
}

/// Process execution identity. Mirrors the gateway's `ProcessPolicy`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProcessPolicy {
    /// User the sandbox process runs as (`sandbox` or a numeric UID).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_as_user: String,

    /// Group the sandbox process runs as (`sandbox` or a numeric GID).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_as_group: String,
}

/// Observed validation state for an `OpenShellPolicy`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenShellPolicyStatus {
    /// Standard conditions; the `Ready` condition is `True` when the document
    /// parsed and validated against the gateway schema, and `False` with the
    /// parser's diagnostic in `message` otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,

    /// `.metadata.generation` last reconciled, for GitOps health checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}
