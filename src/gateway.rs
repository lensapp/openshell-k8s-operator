// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Gateway control-plane abstraction.
//!
//! The reconciler depends on the [`Gateway`] trait rather than the concrete
//! SDK client, so the loop is testable against a fake and the SDK stays a
//! swappable detail (dependency inversion). [`SdkGateway`] is the production
//! implementation backed by `openshell-sdk`.

use async_trait::async_trait;
use openshell_sdk::{ClientConfig, OpenShellClient, SandboxPhase, SandboxSpec, SdkError};

use crate::error::Result;

/// Minimal projection of a gateway sandbox the reconciler consumes.
///
/// Deliberately not the SDK's `SandboxRef`: the reconciler only needs the id
/// and phase, and `SandboxRef` is `#[non_exhaustive]` (no public constructor),
/// which would make the trait impossible to fake in tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxState {
    /// Gateway-assigned sandbox identifier.
    pub id: String,
    /// Current lifecycle phase reported by the gateway.
    pub phase: SandboxPhase,
}

/// The subset of the OpenShell gateway API the reconciler drives.
#[async_trait]
pub trait Gateway: Send + Sync {
    /// Create a sandbox from the given spec.
    async fn create_sandbox(&self, spec: SandboxSpec) -> Result<SandboxState>;

    /// Fetch a sandbox by name, or `None` if the gateway has no such sandbox.
    async fn get_sandbox(&self, name: &str) -> Result<Option<SandboxState>>;

    /// Delete a sandbox by name. Returns `false` if it was already absent.
    async fn delete_sandbox(&self, name: &str) -> Result<bool>;
}

/// [`Gateway`] backed by the real `openshell-sdk` client.
pub struct SdkGateway {
    client: OpenShellClient,
}

impl SdkGateway {
    /// Connect to the gateway at `endpoint`.
    ///
    /// In the co-located deployment topology this is a loopback address and no
    /// auth is attached — the operator and gateway share a pod (see `PLAN.md`).
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let mut config = ClientConfig::new(endpoint);
        config.auth = None;
        let client = OpenShellClient::connect(config).await?;
        Ok(Self { client })
    }
}

#[async_trait]
impl Gateway for SdkGateway {
    async fn create_sandbox(&self, spec: SandboxSpec) -> Result<SandboxState> {
        let sandbox = self.client.create_sandbox(spec).await?;
        Ok(SandboxState {
            id: sandbox.id,
            phase: sandbox.phase,
        })
    }

    async fn get_sandbox(&self, name: &str) -> Result<Option<SandboxState>> {
        match self.client.get_sandbox(name).await {
            Ok(sandbox) => Ok(Some(SandboxState {
                id: sandbox.id,
                phase: sandbox.phase,
            })),
            Err(SdkError::NotFound { .. }) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    async fn delete_sandbox(&self, name: &str) -> Result<bool> {
        Ok(self.client.delete_sandbox(name).await?)
    }
}
