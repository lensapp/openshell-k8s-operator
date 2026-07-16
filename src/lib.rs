// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OpenShell Kubernetes operator.
//!
//! A thin, declarative front-end over an OpenShell gateway's gRPC control
//! plane. Custom resources describe desired state; the reconcilers translate
//! them into gateway calls and mirror gateway state back into `.status`.
//!
//! See `PLAN.md` for the architecture. Milestone 2 adds the `Provider` resource
//! (credentials resolved from a Secret, entitlement-checked, synced to the
//! gateway with a rotation watch) alongside the `OpenShellSandbox` loop.

pub mod controllers;
pub mod crd;
pub mod error;
pub mod gateway;
pub mod secret;

pub use crd::{
    OpenShellSandbox, OpenShellSandboxSpec, OpenShellSandboxStatus, Phase, Provider, ProviderPhase,
    ProviderSpec, ProviderStatus, SecretRef,
};
pub use error::{Error, Result};
