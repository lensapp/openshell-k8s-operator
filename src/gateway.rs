// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Gateway control-plane abstraction.
//!
//! The reconcilers depend on the [`Gateway`] trait rather than the concrete
//! SDK client, so the loops are testable against a fake and the SDK stays a
//! swappable detail (dependency inversion). [`SdkGateway`] is the production
//! implementation backed by `openshell-sdk`.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use openshell_sdk::raw::proto::{
    self, CreateProviderRequest, DeleteProviderRequest, GetProviderRequest, UpdateProviderRequest,
};
use openshell_sdk::{ClientConfig, OpenShellClient, SandboxPhase, SandboxSpec, SdkError};
use tonic::Code;

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

/// Resolved provider desired state handed to the gateway. Credentials are
/// already resolved from the referenced Secret by the caller.
#[derive(Clone, Debug, Default)]
pub struct ProviderInput {
    /// Provider name (matches the CR name).
    pub name: String,
    /// Canonical provider type slug.
    pub provider_type: String,
    /// Resolved credential values.
    pub credentials: BTreeMap<String, String>,
    /// Non-secret configuration.
    pub config: BTreeMap<String, String>,
}

/// The subset of the OpenShell gateway API the reconcilers drive.
#[async_trait]
pub trait Gateway: Send + Sync {
    /// Create a sandbox from the given spec.
    async fn create_sandbox(&self, spec: SandboxSpec) -> Result<SandboxState>;

    /// Fetch a sandbox by name, or `None` if the gateway has no such sandbox.
    async fn get_sandbox(&self, name: &str) -> Result<Option<SandboxState>>;

    /// Delete a sandbox by name. Returns `false` if it was already absent.
    async fn delete_sandbox(&self, name: &str) -> Result<bool>;

    /// Create or update a provider so it matches `input`.
    async fn upsert_provider(&self, input: ProviderInput) -> Result<()>;

    /// Delete a provider by name. Returns `false` if it was already absent.
    async fn delete_provider(&self, name: &str) -> Result<bool>;
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

    async fn upsert_provider(&self, input: ProviderInput) -> Result<()> {
        let mut grpc = self.client.raw_grpc();

        // Fetch any existing provider to decide create-vs-update and carry its
        // metadata (id, resource_version) into the update.
        let existing = match grpc
            .get_provider(GetProviderRequest {
                name: input.name.clone(),
            })
            .await
        {
            Ok(response) => response.into_inner().provider,
            Err(status) if status.code() == Code::NotFound => None,
            Err(status) => return Err(status.into()),
        };

        let metadata = existing
            .as_ref()
            .and_then(|provider| provider.metadata.clone())
            .unwrap_or_else(|| proto::datamodel::v1::ObjectMeta {
                name: input.name.clone(),
                ..proto::datamodel::v1::ObjectMeta::default()
            });

        let provider = proto::Provider {
            metadata: Some(metadata),
            r#type: input.provider_type,
            credentials: input.credentials.into_iter().collect(),
            config: input.config.into_iter().collect(),
            // Credential expiry is not modelled in v1.
            credential_expires_at_ms: HashMap::new(),
        };

        if existing.is_some() {
            grpc.update_provider(UpdateProviderRequest {
                provider: Some(provider),
                credential_expires_at_ms: HashMap::new(),
            })
            .await?;
        } else {
            grpc.create_provider(CreateProviderRequest {
                provider: Some(provider),
            })
            .await?;
        }
        Ok(())
    }

    async fn delete_provider(&self, name: &str) -> Result<bool> {
        let mut grpc = self.client.raw_grpc();
        let response = grpc
            .delete_provider(DeleteProviderRequest {
                name: name.to_owned(),
            })
            .await?;
        Ok(response.into_inner().deleted)
    }
}
