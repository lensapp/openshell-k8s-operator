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
//! entitlement-checked, synced to the gateway with a rotation watch) and
//! `OpenShellPolicy` (a reusable policy document validated by the gateway parser and
//! applied to a sandbox at creation via `spec.policyRef`, or inlined under
//! `spec.policy`).

pub mod controllers;
pub mod crd;
pub mod error;
pub mod gateway;
pub mod policy;
pub mod secret;
pub mod volumes;

pub use crd::{
    FilesystemPolicy, LandlockPolicy, OpenShellPolicy, OpenShellPolicySpec, OpenShellPolicyStatus,
    OpenShellProvider, OpenShellProviderPhase, OpenShellProviderSpec, OpenShellProviderStatus,
    OpenShellSandbox, OpenShellSandboxSpec, OpenShellSandboxStatus, Phase, ProcessPolicy,
    SandboxVolume, SecretRef, VolumeRetention,
};
pub use error::{Error, Result};
