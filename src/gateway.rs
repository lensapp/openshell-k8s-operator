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
    self, CreateProviderRequest, CreateSandboxRequest, DeleteProviderRequest, GetProviderRequest,
    UpdateProviderRequest,
};
use openshell_sdk::{ClientConfig, OpenShellClient, SandboxPhase, SdkError};
use tonic::Code;

use crate::error::{Error, Result};

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

/// Resolved sandbox desired state handed to the gateway.
///
/// Deliberately not the SDK's curated `SandboxSpec`: that type has no policy
/// field, and the full policy (filesystem/landlock/process) must be supplied at
/// create time because those sections are immutable on a running sandbox.
#[derive(Clone, Debug, Default)]
pub struct SandboxCreate {
    /// Sandbox name (matches the CR name).
    pub name: String,
    /// Container image. `None` defers to the gateway default.
    pub image: Option<String>,
    /// Environment variables injected into the sandbox runtime.
    pub environment: BTreeMap<String, String>,
    /// Provider names to attach.
    pub providers: Vec<String>,
    /// Request a GPU.
    pub gpu: bool,
    /// Resolved sandbox policy, already validated by the caller. `None` leaves
    /// the gateway to apply its default policy.
    pub policy: Option<proto::SandboxPolicy>,
    /// Driver-keyed `driver_config` envelope (e.g. custom volume mounts), or
    /// `None`. Passed through to the sandbox template verbatim; the gateway
    /// forwards the block matching the active compute driver.
    pub driver_config: Option<serde_json::Value>,
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
    /// Create a sandbox from the given desired state.
    async fn create_sandbox(&self, create: SandboxCreate) -> Result<SandboxState>;

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
    async fn create_sandbox(&self, create: SandboxCreate) -> Result<SandboxState> {
        // The curated `create_sandbox` cannot carry a policy, so build the raw
        // request. This mirrors the SDK's own request-builder (image into the
        // template, gpu into resource requirements) and adds the policy.
        let request = create_sandbox_request(create);
        let response = self.client.raw_grpc().create_sandbox(request).await?;
        let sandbox = response.into_inner().sandbox.ok_or_else(|| {
            Error::Gateway(SdkError::invalid_config(
                "sandbox missing from gateway response",
            ))
        })?;
        let phase = SandboxPhase::from(sandbox.phase());
        let id = sandbox.metadata.map(|meta| meta.id).unwrap_or_default();
        Ok(SandboxState { id, phase })
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

/// Build the raw `CreateSandboxRequest`. Mirrors the SDK's curated builder
/// (image into the template, gpu into resource requirements) with the addition
/// of the policy field the curated surface omits.
fn create_sandbox_request(create: SandboxCreate) -> CreateSandboxRequest {
    let SandboxCreate {
        name,
        image,
        environment,
        providers,
        gpu,
        policy,
        driver_config,
    } = create;

    // A template is needed when either the image or the driver_config is set;
    // both live on it. An empty image string defers to the gateway default.
    let driver_config = driver_config.map(json_value_to_struct);
    let template = (image.is_some() || driver_config.is_some()).then(|| proto::SandboxTemplate {
        image: image.unwrap_or_default(),
        driver_config,
        ..proto::SandboxTemplate::default()
    });
    let resource_requirements = gpu.then_some(proto::ResourceRequirements {
        gpu: Some(proto::GpuResourceRequirements { count: None }),
    });

    CreateSandboxRequest {
        spec: Some(proto::SandboxSpec {
            environment: environment.into_iter().collect(),
            template,
            policy,
            providers,
            resource_requirements,
            ..proto::SandboxSpec::default()
        }),
        name,
        labels: HashMap::new(),
        annotations: HashMap::new(),
    }
}

/// Convert a JSON value into a `google.protobuf.Struct`. `driver_config` is
/// always a JSON object; a non-object collapses to an empty struct.
fn json_value_to_struct(value: serde_json::Value) -> prost_types::Struct {
    let serde_json::Value::Object(map) = value else {
        return prost_types::Struct::default();
    };
    prost_types::Struct {
        fields: map
            .into_iter()
            .map(|(key, value)| (key, json_value_to_prost(value)))
            .collect(),
    }
}

/// Recursively convert a JSON value into a `google.protobuf.Value`.
fn json_value_to_prost(value: serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;

    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(flag) => Kind::BoolValue(flag),
        serde_json::Value::Number(num) => Kind::NumberValue(num.as_f64().unwrap_or_default()),
        serde_json::Value::String(text) => Kind::StringValue(text),
        serde_json::Value::Array(items) => Kind::ListValue(prost_types::ListValue {
            values: items.into_iter().map(json_value_to_prost).collect(),
        }),
        serde_json::Value::Object(_) => Kind::StructValue(json_value_to_struct(value)),
    };
    prost_types::Value { kind: Some(kind) }
}

#[cfg(test)]
mod tests {
    use super::{create_sandbox_request, json_value_to_struct};
    use crate::gateway::SandboxCreate;
    use serde_json::json;

    #[test]
    fn json_value_to_struct_maps_nested_scalars() {
        // Numbers use a float literal: `google.protobuf.Struct` stores every
        // number as an f64, so an integer would round-trip as `N.0`.
        let value = json!({
            "kubernetes": {
                "volumes": [{ "name": "data", "read_only": false }],
                "ratio": 2.5,
            }
        });
        let converted = json_value_to_struct(value.clone());

        // Round-tripping back through the gateway's own decoder must reproduce
        // the input, proving the conversion is faithful.
        let round_tripped = openshell_core::proto_struct::struct_to_json_value(&converted);
        assert_eq!(round_tripped, value);
    }

    #[test]
    fn create_request_carries_driver_config_on_template() {
        let create = SandboxCreate {
            name: "box".to_owned(),
            driver_config: Some(json!({ "kubernetes": { "volumes": [] } })),
            ..SandboxCreate::default()
        };
        let request = create_sandbox_request(create);
        let template = request
            .spec
            .and_then(|spec| spec.template)
            .expect("template present when driver_config set");
        assert!(template.driver_config.is_some());
    }
}
