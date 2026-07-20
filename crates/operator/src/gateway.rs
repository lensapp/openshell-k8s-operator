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
    self, AttachSandboxProviderRequest, CreateProviderRequest, CreateSandboxRequest,
    DeleteProviderRequest, DetachSandboxProviderRequest, GetProviderRequest, GetSandboxRequest,
    UpdateConfigRequest, UpdateProviderRequest,
};
use openshell_sdk::{AuthConfig, ClientConfig, OpenShellClient, SandboxPhase, SdkError};
use tonic::Code;

use crate::credentials::{MaterialSpec, RefreshPlan, RefreshSpec, RefreshStrategy};
use crate::error::{Error, Result};

/// Projection of a gateway sandbox the reconciler consumes.
///
/// Deliberately not the SDK's `SandboxRef`: the reconciler needs the current
/// providers to converge them, and `SandboxRef` is `#[non_exhaustive]` (no
/// public constructor), which would make the trait impossible to fake in tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxState {
    /// Gateway-assigned sandbox identifier.
    pub id: String,
    /// Current lifecycle phase reported by the gateway.
    pub phase: SandboxPhase,
    /// Providers currently attached, as the gateway reports them.
    pub providers: Vec<String>,
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
    /// Number of GPUs when `gpu` is set; `None` uses the driver default.
    pub gpu_count: Option<u32>,
    /// Resolved sandbox policy, already validated by the caller. `None` leaves
    /// the gateway to apply its default policy.
    pub policy: Option<proto::SandboxPolicy>,
    /// Driver-keyed `driver_config` envelope (e.g. custom volume mounts), or
    /// `None`. Passed through to the sandbox template verbatim; the gateway
    /// forwards the block matching the active compute driver.
    pub driver_config: Option<serde_json::Value>,
    /// Sandbox-runtime log level, or `None` for the gateway default.
    pub log_level: Option<String>,
    /// Compute resources in the Kubernetes `requests`/`limits` JSON shape, or
    /// `None`. Forwarded to the sandbox template's `resources`.
    pub resources: Option<serde_json::Value>,
    /// `RuntimeClass` name, or `None` for the platform default.
    pub runtime_class_name: Option<String>,
    /// Labels applied to the sandbox's compute-platform resources.
    pub labels: BTreeMap<String, String>,
    /// Annotations applied to the sandbox's compute-platform resources.
    pub annotations: BTreeMap<String, String>,
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

/// A provider-type profile projected to what credential-strategy selection
/// needs.
///
/// Deliberately not the SDK's `ProviderProfile`: that type is
/// `#[non_exhaustive]` (no public constructor, so it can't be faked in tests)
/// and carries far more than credential-handling selection consumes.
#[derive(Clone, Debug, Default)]
pub struct ProviderProfileView {
    /// Profile id (the canonical provider-type slug).
    pub id: String,
    /// Declared credentials.
    pub credentials: Vec<ProviderProfileCredential>,
}

/// One credential a provider profile declares.
#[derive(Clone, Debug, Default)]
pub struct ProviderProfileCredential {
    /// Credential name (the key used with the gateway, e.g. `api_key`).
    pub name: String,
    /// Gateway-minted refresh behaviour, when the profile declares a
    /// gateway-mintable strategy; `None` for static/external/no refresh.
    pub refresh: Option<RefreshSpec>,
}

/// A gateway-minted credential-refresh configuration for one provider
/// credential. The gateway mints short-lived tokens from the supplied
/// [`RefreshPlan`]'s seed material.
#[derive(Clone, Debug)]
pub struct ConfigureRefreshInput {
    /// Provider name.
    pub provider: String,
    /// Credential key within the provider (matches the profile credential name).
    pub credential_key: String,
    /// Strategy and seed material, resolved by [`crate::credentials`].
    pub plan: RefreshPlan,
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

    /// Attach a provider to a live sandbox.
    async fn attach_provider(&self, sandbox: &str, provider: &str) -> Result<()>;

    /// Detach a provider from a live sandbox.
    async fn detach_provider(&self, sandbox: &str, provider: &str) -> Result<()>;

    /// Apply a policy to a live sandbox in place (the gateway's `UpdateConfig`).
    ///
    /// Only the mutable sections (`networkPolicies`, additive `filesystem`) may
    /// actually change; a non-additive `filesystem` edit is rejected as
    /// [`Error::PolicyUpdateRejected`]. `landlock`/`process` never reach this
    /// path — the reconciler recreates the sandbox for those.
    async fn update_policy(&self, sandbox: &str, policy: proto::SandboxPolicy) -> Result<()>;

    /// Create or update a provider so it matches `input`.
    async fn upsert_provider(&self, input: ProviderInput) -> Result<()>;

    /// Delete a provider by name. Returns `false` if it was already absent.
    async fn delete_provider(&self, name: &str) -> Result<bool>;

    /// List the provider-type profiles the gateway knows, projected to
    /// [`ProviderProfileView`]. Used to decide credential handling per type
    /// (which credentials support a gateway-minted refresh strategy).
    async fn list_provider_profiles(&self) -> Result<Vec<ProviderProfileView>>;

    /// Configure gateway-minted credential refresh for one provider credential.
    /// The gateway thereafter mints short-lived tokens from the seed material
    /// rather than injecting a stored static value.
    async fn configure_provider_refresh(&self, input: ConfigureRefreshInput) -> Result<()>;
}

/// Default gateway endpoint when `OPENSHELL_GATEWAY_ENDPOINT` is unset.
const DEFAULT_GATEWAY_ENDPOINT: &str = "http://127.0.0.1:8080";

/// How the operator reaches and authenticates to the gateway.
///
/// The operator authenticates as an OIDC `User` (admin) with a long-lived
/// bearer minted by the bundled issuer (see `docs/operator-auth.md`). Resolved
/// from the environment the chart injects; file-backed fields are read once at
/// startup.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct GatewayConfig {
    /// Gateway URL, e.g. `https://gateway.openshell-system.svc:8080`.
    pub endpoint: String,
    /// Bearer token. `None` connects anonymously (only usable against a gateway
    /// that allows unauthenticated access).
    pub token: Option<String>,
    /// PEM CA bundle for a private-CA gateway. `None` uses the system roots.
    pub ca_cert: Option<Vec<u8>>,
    /// Skip TLS verification (development only).
    pub insecure_skip_verify: bool,
}

impl GatewayConfig {
    /// Resolve the connection from the environment, reading the mounted token
    /// and CA files if their paths are set.
    ///
    /// - `OPENSHELL_GATEWAY_ENDPOINT` — gateway URL (default loopback).
    /// - `OPENSHELL_TOKEN_FILE` — path to the mounted bearer token; unset means
    ///   connect without credentials.
    /// - `OPENSHELL_CA_FILE` — path to the gateway's PEM CA bundle.
    /// - `OPENSHELL_INSECURE_SKIP_VERIFY` — `true` to skip TLS verification.
    pub fn from_env() -> Result<Self> {
        let endpoint = std::env::var("OPENSHELL_GATEWAY_ENDPOINT")
            .unwrap_or_else(|_| DEFAULT_GATEWAY_ENDPOINT.to_string());
        let token = read_optional_file("OPENSHELL_TOKEN_FILE")?.map(|t| t.trim().to_string());
        let ca_cert = read_optional_file("OPENSHELL_CA_FILE")?.map(String::into_bytes);
        let insecure_skip_verify = std::env::var("OPENSHELL_INSECURE_SKIP_VERIFY")
            .is_ok_and(|v| v.eq_ignore_ascii_case("true"));
        Ok(Self {
            endpoint,
            token,
            ca_cert,
            insecure_skip_verify,
        })
    }
}

/// Read the file named by env var `key`, or `None` if the var is unset.
fn read_optional_file(key: &str) -> Result<Option<String>> {
    std::env::var(key)
        .ok()
        .map(|path| read_file(key, &path))
        .transpose()
}

/// Read `path`, tagging any I/O error with the env var it came from.
fn read_file(key: &str, path: &str) -> Result<String> {
    std::fs::read_to_string(path).map_err(|source| {
        Error::Gateway(SdkError::invalid_config(format!(
            "reading {key} ({path}): {source}"
        )))
    })
}

/// [`Gateway`] backed by the real `openshell-sdk` client.
pub struct SdkGateway {
    client: OpenShellClient,
}

impl SdkGateway {
    /// Connect to the gateway described by `config`.
    ///
    /// When a token is present it is sent as an OIDC bearer over server-TLS
    /// (the gateway requires no client cert with OIDC configured); otherwise
    /// the client connects anonymously.
    pub async fn connect(config: GatewayConfig) -> Result<Self> {
        let mut client_config = ClientConfig::new(config.endpoint);
        client_config.auth = config.token.map(AuthConfig::oidc);
        client_config.ca_cert = config.ca_cert;
        client_config.insecure_skip_verify = config.insecure_skip_verify;
        let client = OpenShellClient::connect(client_config).await?;
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
        Ok(sandbox_state(&sandbox))
    }

    async fn get_sandbox(&self, name: &str) -> Result<Option<SandboxState>> {
        // Use the raw client so the full spec (current providers) and metadata
        // (resource version) come back for convergence — the curated
        // `get_sandbox` projects those away.
        let mut grpc = self.client.raw_grpc();
        match grpc
            .get_sandbox(GetSandboxRequest {
                name: name.to_owned(),
            })
            .await
        {
            Ok(response) => Ok(response.into_inner().sandbox.as_ref().map(sandbox_state)),
            Err(status) if status.code() == Code::NotFound => Ok(None),
            Err(status) => Err(status.into()),
        }
    }

    async fn delete_sandbox(&self, name: &str) -> Result<bool> {
        Ok(self.client.delete_sandbox(name).await?)
    }

    async fn attach_provider(&self, sandbox: &str, provider: &str) -> Result<()> {
        self.client
            .raw_grpc()
            .attach_sandbox_provider(AttachSandboxProviderRequest {
                sandbox_name: sandbox.to_owned(),
                provider_name: provider.to_owned(),
                // 0 → the gateway applies against the current resource version.
                // The operator is the sole writer of a sandbox and may change
                // several providers in one reconcile (each attach bumps the
                // version), so pinning a pre-read version would self-conflict.
                expected_resource_version: 0,
            })
            .await?;
        Ok(())
    }

    async fn detach_provider(&self, sandbox: &str, provider: &str) -> Result<()> {
        self.client
            .raw_grpc()
            .detach_sandbox_provider(DetachSandboxProviderRequest {
                sandbox_name: sandbox.to_owned(),
                provider_name: provider.to_owned(),
                expected_resource_version: 0,
            })
            .await?;
        Ok(())
    }

    async fn update_policy(&self, sandbox: &str, policy: proto::SandboxPolicy) -> Result<()> {
        let request = UpdateConfigRequest {
            name: sandbox.to_owned(),
            policy: Some(policy),
            // 0 → apply against the current resource version; the operator is the
            // sole writer, matching attach/detach above.
            expected_resource_version: 0,
            ..UpdateConfigRequest::default()
        };
        match self.client.raw_grpc().update_config(request).await {
            Ok(_) => Ok(()),
            // The gateway signals a forbidden (non-additive) policy change with
            // `InvalidArgument`. Surface it as a terminal error so it doesn't
            // hot-loop; anything else is a transient RPC failure worth retrying.
            Err(status) if status.code() == Code::InvalidArgument => {
                Err(Error::PolicyUpdateRejected(status.message().to_owned()))
            }
            Err(status) => Err(status.into()),
        }
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

    async fn list_provider_profiles(&self) -> Result<Vec<ProviderProfileView>> {
        let mut grpc = self.client.raw_grpc();
        let response = grpc
            .list_provider_profiles(proto::ListProviderProfilesRequest::default())
            .await?;
        Ok(response
            .into_inner()
            .profiles
            .into_iter()
            .map(provider_profile_view)
            .collect())
    }

    async fn configure_provider_refresh(&self, input: ConfigureRefreshInput) -> Result<()> {
        let mut grpc = self.client.raw_grpc();
        grpc.configure_provider_refresh(proto::ConfigureProviderRefreshRequest {
            provider: input.provider,
            credential_key: input.credential_key,
            strategy: refresh_strategy_to_proto(input.plan.strategy) as i32,
            material: input.plan.material.into_iter().collect(),
            secret_material_keys: input.plan.secret_material_keys,
            // The credential's own expiry is managed by the refresh loop.
            expires_at_ms: None,
        })
        .await?;
        Ok(())
    }
}

/// Project a raw proto `ProviderProfile` onto [`ProviderProfileView`].
fn provider_profile_view(profile: proto::ProviderProfile) -> ProviderProfileView {
    ProviderProfileView {
        id: profile.id,
        credentials: profile
            .credentials
            .into_iter()
            .map(profile_credential)
            .collect(),
    }
}

/// Project a raw proto `ProviderProfileCredential` onto the reconciler's view,
/// keeping only a refresh spec the gateway can actually mint.
fn profile_credential(credential: proto::ProviderProfileCredential) -> ProviderProfileCredential {
    ProviderProfileCredential {
        name: credential.name,
        refresh: credential.refresh.and_then(refresh_spec),
    }
}

/// Map a proto refresh block onto a [`RefreshSpec`], or `None` when its strategy
/// is not gateway-mintable (`unspecified`/`static`/`external`).
fn refresh_spec(refresh: proto::ProviderCredentialRefresh) -> Option<RefreshSpec> {
    let strategy = refresh_strategy_from_proto(refresh.strategy)?;
    Some(RefreshSpec {
        strategy,
        material: refresh
            .material
            .into_iter()
            .map(|material| MaterialSpec {
                name: material.name,
                required: material.required,
                secret: material.secret,
            })
            .collect(),
    })
}

/// Map a proto refresh-strategy discriminant onto a gateway-mintable
/// [`RefreshStrategy`], or `None` for strategies the gateway does not mint.
fn refresh_strategy_from_proto(value: i32) -> Option<RefreshStrategy> {
    use proto::ProviderCredentialRefreshStrategy as S;
    match S::try_from(value).ok()? {
        S::Oauth2RefreshToken => Some(RefreshStrategy::Oauth2RefreshToken),
        S::Oauth2ClientCredentials => Some(RefreshStrategy::Oauth2ClientCredentials),
        S::GoogleServiceAccountJwt => Some(RefreshStrategy::GoogleServiceAccountJwt),
        S::AwsStsAssumeRole => Some(RefreshStrategy::AwsStsAssumeRole),
        S::Unspecified | S::Static | S::External => None,
    }
}

/// Map a [`RefreshStrategy`] onto its proto discriminant.
fn refresh_strategy_to_proto(
    strategy: RefreshStrategy,
) -> proto::ProviderCredentialRefreshStrategy {
    use proto::ProviderCredentialRefreshStrategy as S;
    match strategy {
        RefreshStrategy::Oauth2RefreshToken => S::Oauth2RefreshToken,
        RefreshStrategy::Oauth2ClientCredentials => S::Oauth2ClientCredentials,
        RefreshStrategy::GoogleServiceAccountJwt => S::GoogleServiceAccountJwt,
        RefreshStrategy::AwsStsAssumeRole => S::AwsStsAssumeRole,
    }
}

/// Project a raw proto `Sandbox` onto the reconciler's [`SandboxState`].
fn sandbox_state(sandbox: &proto::Sandbox) -> SandboxState {
    let phase = SandboxPhase::from(sandbox.phase());
    let id = sandbox
        .metadata
        .as_ref()
        .map(|meta| meta.id.clone())
        .unwrap_or_default();
    let providers = sandbox
        .spec
        .as_ref()
        .map(|spec| spec.providers.clone())
        .unwrap_or_default();
    SandboxState {
        id,
        phase,
        providers,
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
        gpu_count,
        policy,
        driver_config,
        log_level,
        resources,
        runtime_class_name,
        labels,
        annotations,
    } = create;

    // A template is needed once any template-level field is set. An empty image
    // string defers to the gateway default.
    let driver_config = driver_config.map(json_value_to_struct);
    let resources = resources.map(json_value_to_struct);
    let needs_template = image.is_some()
        || driver_config.is_some()
        || resources.is_some()
        || runtime_class_name.is_some()
        || !labels.is_empty()
        || !annotations.is_empty();
    let template = needs_template.then(|| proto::SandboxTemplate {
        image: image.unwrap_or_default(),
        runtime_class_name: runtime_class_name.unwrap_or_default(),
        labels: labels.into_iter().collect(),
        annotations: annotations.into_iter().collect(),
        resources,
        driver_config,
        ..proto::SandboxTemplate::default()
    });
    let resource_requirements = gpu.then_some(proto::ResourceRequirements {
        gpu: Some(proto::GpuResourceRequirements { count: gpu_count }),
    });

    CreateSandboxRequest {
        spec: Some(proto::SandboxSpec {
            log_level: log_level.unwrap_or_default(),
            environment: environment.into_iter().collect(),
            template,
            policy,
            providers,
            resource_requirements,
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
    use super::{create_sandbox_request, json_value_to_struct, read_file};
    use crate::gateway::SandboxCreate;
    use serde_json::json;

    #[test]
    fn read_file_returns_contents_for_trimming() {
        // A mounted Secret file often carries a trailing newline; `from_env`
        // trims it, so a token that round-trips through a file still matches.
        let path = std::env::temp_dir().join("openshell-test-token");
        std::fs::write(&path, "the-token\n").expect("write temp token");
        let contents = read_file("OPENSHELL_TOKEN_FILE", path.to_str().unwrap()).expect("read");
        assert_eq!(contents.trim(), "the-token");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_file_errors_on_missing_path() {
        // "Set but unreadable" is a hard error, distinct from "unset" (None).
        let err = read_file("OPENSHELL_CA_FILE", "/nonexistent/openshell/ca.crt");
        assert!(err.is_err());
    }

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

    #[test]
    fn create_request_carries_new_primitives() {
        let create = SandboxCreate {
            name: "box".to_owned(),
            gpu: true,
            gpu_count: Some(3),
            log_level: Some("debug".to_owned()),
            runtime_class_name: Some("gvisor".to_owned()),
            labels: std::collections::BTreeMap::from([("team".to_owned(), "core".to_owned())]),
            annotations: std::collections::BTreeMap::from([("k".to_owned(), "v".to_owned())]),
            resources: Some(json!({ "requests": { "cpu": "500m" } })),
            ..SandboxCreate::default()
        };
        let spec = create_sandbox_request(create).spec.expect("spec present");
        assert_eq!(spec.log_level, "debug");
        assert_eq!(
            spec.resource_requirements
                .and_then(|r| r.gpu)
                .and_then(|g| g.count),
            Some(3)
        );
        let template = spec.template.expect("template present");
        assert_eq!(template.runtime_class_name, "gvisor");
        assert_eq!(
            template.labels.get("team").map(String::as_str),
            Some("core")
        );
        assert_eq!(template.annotations.get("k").map(String::as_str), Some("v"));
        assert!(template.resources.is_some());
    }
}
