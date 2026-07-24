// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OpenShell Kubernetes operator.
//!
//! A thin, declarative front-end over an OpenShell gateway's gRPC control
//! plane. Custom resources describe desired state; the reconcilers translate
//! them into gateway calls and mirror gateway state back into `.status`.
//!
//! See `PLAN.md` for the architecture. Alongside the `OpenShellSandbox` loop,
//! the operator reconciles `OpenShellProvider` (credentials resolved from a Secret,
//! entitlement-checked, synced to the gateway with a rotation watch),
//! `OpenShellPolicy` (a reusable policy document validated by the gateway parser and
//! applied to a sandbox at creation via `spec.policyRef`, or inlined under
//! `spec.policy`), and `OpenShellWorkspace` (a cluster-scoped gateway tenancy
//! boundary with declarative membership, that sandboxes and providers join via
//! `spec.workspace`).

pub mod conditions;
pub mod controllers;
pub mod crd;
pub mod credentials;
pub mod error;
pub mod gateway;
pub mod health;
pub mod leader;
pub mod policy;
pub mod secret;
pub mod volumes;
pub mod webhook;

pub use crd::{
    FilesystemPolicy, LandlockPolicy, OpenShellPolicy, OpenShellPolicySpec, OpenShellPolicyStatus,
    OpenShellProvider, OpenShellProviderSpec, OpenShellProviderStatus, OpenShellSandbox,
    OpenShellSandboxSpec, OpenShellSandboxStatus, OpenShellWorkspace, OpenShellWorkspaceSpec,
    OpenShellWorkspaceStatus, Phase, ProcessPolicy, ResourceQuantities, SandboxResources,
    SandboxVolume, SecretRef, VolumeRetention, WorkspaceMember, WorkspacePhase, WorkspaceRole,
};
pub use error::{Error, Result};
