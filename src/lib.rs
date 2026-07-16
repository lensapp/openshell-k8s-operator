// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OpenShell Kubernetes operator.
//!
//! A thin, declarative front-end over an OpenShell gateway's gRPC control
//! plane. Custom resources describe desired state; the reconciler translates
//! them into gateway calls and mirrors gateway state back into `.status`.
//!
//! See `PLAN.md` for the architecture. Milestone 1 covers the `OpenShellSandbox`
//! resource and its create/get/delete reconcile loop over a loopback gateway.

pub mod controller;
pub mod crd;
pub mod error;
pub mod gateway;

pub use crd::{OpenShellSandbox, OpenShellSandboxSpec, OpenShellSandboxStatus, Phase};
pub use error::{Error, Result};
