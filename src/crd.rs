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
