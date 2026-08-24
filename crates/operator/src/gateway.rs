// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Gateway control-plane abstraction.
//!
//! The reconcilers depend on the [`Gateway`] trait rather than the concrete
//! SDK client, so the loops are testable against a fake and the SDK stays a
//! swappable detail (dependency inversion). [`SdkGateway`] is the production
//! implementation backed by `openshell-sdk`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use openshell_sdk::raw::AuthedGrpcClient;
use openshell_sdk::raw::proto::{
    self, AddWorkspaceMemberRequest, AttachSandboxProviderRequest, CreateProviderRequest,
    CreateSandboxRequest, CreateWorkspaceRequest, DeleteProviderRequest, DeleteSandboxRequest,
    DeleteWorkspaceRequest, DetachSandboxProviderRequest, GetProviderRequest, GetSandboxRequest,
    GetWorkspaceRequest, ListWorkspaceMembersRequest, RemoveWorkspaceMemberRequest,
    UpdateConfigRequest, UpdateProviderRequest,
};
use openshell_sdk::{
    AuthConfig, ClientConfig, OpenShellClient, Refresh, RefreshError, RefreshedToken, SandboxPhase,
    SdkError,
};
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
    /// Gateway workspace the sandbox belongs to. Empty is the gateway's
    /// `default` workspace (its own normalization), preserving prior behaviour.
    pub workspace: String,
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
    /// Gateway workspace the provider belongs to. Empty is the gateway's
    /// `default` workspace.
    pub workspace: String,
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
    /// Gateway workspace the provider belongs to. Empty is the gateway's
    /// `default` workspace.
    pub workspace: String,
}

/// Resolved workspace desired state handed to the gateway at creation.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceCreate {
    /// Workspace name (matches the CR name); a DNS-1123 label.
    pub name: String,
    /// Labels applied at creation. The gateway has no update RPC, so these are
    /// create-time only.
    pub labels: BTreeMap<String, String>,
}

/// Projection of a gateway workspace the reconciler consumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceState {
    /// Current lifecycle phase reported by the gateway.
    pub phase: WorkspacePhase,
}

/// Lifecycle phase of a gateway workspace, projected for the reconciler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkspacePhase {
    /// Workspace is active and usable.
    Active,
    /// Workspace is being torn down.
    Terminating,
    /// Any other/unspecified gateway phase.
    Unknown,
}

/// A workspace member as the gateway reports it, projected for convergence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceMemberView {
    /// OIDC subject claim identifying the principal.
    pub subject: String,
    /// Role the principal holds in the workspace.
    pub role: WorkspaceRole,
}

/// Role a workspace member holds. Mirrors the gateway's `WorkspaceRole`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkspaceRole {
    /// Regular member.
    User,
    /// Workspace administrator.
    Admin,
}

/// The subset of the OpenShell gateway API the reconcilers drive.
#[async_trait]
pub trait Gateway: Send + Sync {
    /// Create a sandbox from the given desired state.
    async fn create_sandbox(&self, create: SandboxCreate) -> Result<SandboxState>;

    /// Fetch a sandbox by name within `workspace`, or `None` if the gateway has
    /// no such sandbox. Empty `workspace` is the gateway's `default`.
    async fn get_sandbox(&self, name: &str, workspace: &str) -> Result<Option<SandboxState>>;

    /// Delete a sandbox by name within `workspace`. Returns `false` if it was
    /// already absent.
    async fn delete_sandbox(&self, name: &str, workspace: &str) -> Result<bool>;

    /// Attach a provider to a live sandbox in `workspace`.
    async fn attach_provider(&self, sandbox: &str, provider: &str, workspace: &str) -> Result<()>;

    /// Detach a provider from a live sandbox in `workspace`.
    async fn detach_provider(&self, sandbox: &str, provider: &str, workspace: &str) -> Result<()>;

    /// Apply a policy to a live sandbox in place (the gateway's `UpdateConfig`).
    ///
    /// Only the mutable sections (`networkPolicies`, additive `filesystem`) may
    /// actually change; a non-additive `filesystem` edit is rejected as
    /// [`Error::PolicyUpdateRejected`]. `landlock`/`process` never reach this
    /// path — the reconciler recreates the sandbox for those.
    async fn update_policy(
        &self,
        sandbox: &str,
        policy: proto::SandboxPolicy,
        workspace: &str,
    ) -> Result<()>;

    /// Create or update a provider so it matches `input`.
    async fn upsert_provider(&self, input: ProviderInput) -> Result<()>;

    /// Delete a provider by name within `workspace`. Returns `false` if it was
    /// already absent.
    async fn delete_provider(&self, name: &str, workspace: &str) -> Result<bool>;

    /// List the provider-type profiles the gateway knows, projected to
    /// [`ProviderProfileView`]. Used to decide credential handling per type
    /// (which credentials support a gateway-minted refresh strategy).
    async fn list_provider_profiles(&self) -> Result<Vec<ProviderProfileView>>;

    /// Configure gateway-minted credential refresh for one provider credential.
    /// The gateway thereafter mints short-lived tokens from the seed material
    /// rather than injecting a stored static value.
    async fn configure_provider_refresh(&self, input: ConfigureRefreshInput) -> Result<()>;

    /// Import or update a platform-scoped provider-type profile so the gateway
    /// matches `profile`. Imports when no managed custom profile of that id
    /// exists yet; otherwise updates it with optimistic concurrency on the
    /// gateway's current stored resource version. Returns the stored resource
    /// version after the write.
    async fn upsert_provider_profile(&self, profile: proto::ProviderProfile) -> Result<u64>;

    /// Delete a platform-scoped provider profile by id. Returns `false` if it
    /// was already absent.
    async fn delete_provider_profile(&self, id: &str) -> Result<bool>;

    /// Create a workspace from the given desired state. Idempotent: an existing
    /// workspace of the same name is adopted (its state returned) rather than
    /// erroring, so a CR can also take over a workspace created out of band.
    async fn create_workspace(&self, create: WorkspaceCreate) -> Result<WorkspaceState>;

    /// Fetch a workspace by name, or `None` if the gateway has no such workspace.
    async fn get_workspace(&self, name: &str) -> Result<Option<WorkspaceState>>;

    /// Delete a workspace by name. Returns `false` if it was already absent.
    async fn delete_workspace(&self, name: &str) -> Result<bool>;

    /// List a workspace's members, projected to [`WorkspaceMemberView`].
    async fn list_workspace_members(&self, workspace: &str) -> Result<Vec<WorkspaceMemberView>>;

    /// Grant a principal (by OIDC subject) a role in a workspace. The gateway's
    /// add is create-only, so a role change is a remove followed by an add.
    async fn add_workspace_member(
        &self,
        workspace: &str,
        subject: &str,
        role: WorkspaceRole,
    ) -> Result<()>;

    /// Remove a principal (by OIDC subject) from a workspace.
    async fn remove_workspace_member(&self, workspace: &str, subject: &str) -> Result<()>;
}

/// Default gateway endpoint when `OPENSHELL_GATEWAY_ENDPOINT` is unset.
const DEFAULT_GATEWAY_ENDPOINT: &str = "http://127.0.0.1:8080";

/// How the operator reaches and authenticates to the gateway.
///
/// The operator authenticates as an OIDC `User` (admin) with a long-lived
/// bearer minted by the bundled issuer (see `docs/operator-auth.md`). Resolved
/// from the environment the chart injects. The CA is read once at startup; the
/// bearer token is re-read on demand (see [`FileTokenRefresher`]) so a rotated
/// token is picked up without restarting the operator.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct GatewayConfig {
    /// Gateway URL, e.g. `https://gateway.openshell-system.svc:8080`.
    pub endpoint: String,
    /// Bearer token. `None` connects anonymously (only usable against a gateway
    /// that allows unauthenticated access).
    pub token: Option<String>,
    /// Path the bearer was read from, if any. Retained so the client can
    /// re-read it as the platform rotates the token (a projected `ServiceAccount`
    /// token, or a Secret an external refresher rewrites).
    pub token_path: Option<String>,
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
        let token_path = std::env::var("OPENSHELL_TOKEN_FILE").ok();
        let token = token_path
            .as_deref()
            .map(|path| read_file("OPENSHELL_TOKEN_FILE", path))
            .transpose()?
            .map(|t| t.trim().to_string());
        let ca_cert = read_optional_file("OPENSHELL_CA_FILE")?.map(String::into_bytes);
        let insecure_skip_verify = std::env::var("OPENSHELL_INSECURE_SKIP_VERIFY")
            .is_ok_and(|v| v.eq_ignore_ascii_case("true"));
        Ok(Self {
            endpoint,
            token,
            token_path,
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

/// How long the SDK may cache the bearer before re-reading the token file.
///
/// This is a poll interval, not the token's real lifetime: the mounted file is
/// re-read this often (minus the SDK's refresh skew) so a rotated token reaches
/// the live bearer slot before the previous one expires. Kept short because
/// re-reading a local file is cheap, and short enough to cover the smallest
/// projected-token TTL Kubernetes allows.
const TOKEN_RECHECK: Duration = Duration::from_secs(120);

/// Absolute time (Unix seconds) at which a just-read token should be re-read.
fn recheck_at() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_add(TOKEN_RECHECK.as_secs())
}

/// [`Refresh`] that re-reads the bearer from its mounted file.
///
/// The operator's token is a file the platform keeps current — a projected
/// `ServiceAccount` token the kubelet rotates, or a Secret an external refresher
/// rewrites — so "refreshing" is re-reading the file, not talking to an `IdP`.
/// The SDK drives this proactively before the advertised expiry, so a
/// long-running operator picks up a rotated token without a restart.
struct FileTokenRefresher {
    path: String,
}

#[async_trait]
impl Refresh for FileTokenRefresher {
    async fn refresh(&self) -> std::result::Result<RefreshedToken, RefreshError> {
        // A missing/half-written file is transient — the kubelet may be
        // mid-rotation — so the SDK should retry rather than give up.
        let token = std::fs::read_to_string(&self.path).map_err(|source| {
            RefreshError::Transient(format!("re-reading token file {}: {source}", self.path))
        })?;
        Ok(RefreshedToken::new(token.trim()).with_expires_at(recheck_at()))
    }
}

/// Build the SDK auth config from the resolved token.
///
/// A file-backed token wires a [`FileTokenRefresher`] so the bearer rotates in
/// place; a token with no known file (or none at all) stays static.
fn build_auth(token: Option<String>, token_path: Option<String>) -> Option<AuthConfig> {
    match (token, token_path) {
        (Some(token), Some(path)) => Some(AuthConfig::Oidc {
            token,
            expires_at: Some(recheck_at()),
            refresh: Some(Arc::new(FileTokenRefresher { path })),
        }),
        (Some(token), None) => Some(AuthConfig::oidc(token)),
        (None, _) => None,
    }
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
        client_config.auth = build_auth(config.token, config.token_path);
        client_config.ca_cert = config.ca_cert;
        client_config.insecure_skip_verify = config.insecure_skip_verify;
        let client = OpenShellClient::connect(client_config).await?;
        Ok(Self { client })
    }

    /// Authenticated raw gRPC client with a proactively-refreshed bearer.
    ///
    /// Unlike `raw_grpc`, this re-reads a rotated token into the shared bearer
    /// slot before the old one expires (see [`FileTokenRefresher`]). Every raw
    /// RPC dials through here, so a long-lived operator is never pinned to the
    /// token it read at startup.
    async fn grpc(&self) -> Result<AuthedGrpcClient> {
        Ok(self.client.raw_grpc_fresh().await?)
    }
}

#[async_trait]
impl Gateway for SdkGateway {
    async fn create_sandbox(&self, create: SandboxCreate) -> Result<SandboxState> {
        // The curated `create_sandbox` cannot carry a policy, so build the raw
        // request. This mirrors the SDK's own request-builder (image into the
        // template, gpu into resource requirements) and adds the policy.
        let request = create_sandbox_request(create);
        let response = self.grpc().await?.create_sandbox(request).await?;
        let sandbox = response.into_inner().sandbox.ok_or_else(|| {
            Error::Gateway(SdkError::invalid_config(
                "sandbox missing from gateway response",
            ))
        })?;
        Ok(sandbox_state(&sandbox))
    }

    async fn get_sandbox(&self, name: &str, workspace: &str) -> Result<Option<SandboxState>> {
        // Use the raw client so the full spec (current providers) and metadata
        // (resource version) come back for convergence — the curated
        // `get_sandbox` projects those away.
        let mut grpc = self.grpc().await?;
        match grpc
            .get_sandbox(GetSandboxRequest {
                name: name.to_owned(),
                workspace: workspace.to_owned(),
            })
            .await
        {
            Ok(response) => Ok(response.into_inner().sandbox.as_ref().map(sandbox_state)),
            Err(status) if status.code() == Code::NotFound => Ok(None),
            Err(status) => Err(status.into()),
        }
    }

    async fn delete_sandbox(&self, name: &str, workspace: &str) -> Result<bool> {
        // Raw (not the curated `delete_sandbox`, which is fixed to the default
        // workspace) so the delete targets the sandbox's actual workspace.
        let response = self
            .grpc()
            .await?
            .delete_sandbox(DeleteSandboxRequest {
                name: name.to_owned(),
                workspace: workspace.to_owned(),
            })
            .await?;
        Ok(response.into_inner().deleted)
    }

    async fn attach_provider(&self, sandbox: &str, provider: &str, workspace: &str) -> Result<()> {
        self.grpc()
            .await?
            .attach_sandbox_provider(AttachSandboxProviderRequest {
                sandbox_name: sandbox.to_owned(),
                provider_name: provider.to_owned(),
                // 0 → the gateway applies against the current resource version.
                // The operator is the sole writer of a sandbox and may change
                // several providers in one reconcile (each attach bumps the
                // version), so pinning a pre-read version would self-conflict.
                expected_resource_version: 0,
                workspace: workspace.to_owned(),
            })
            .await?;
        Ok(())
    }

    async fn detach_provider(&self, sandbox: &str, provider: &str, workspace: &str) -> Result<()> {
        self.grpc()
            .await?
            .detach_sandbox_provider(DetachSandboxProviderRequest {
                sandbox_name: sandbox.to_owned(),
                provider_name: provider.to_owned(),
                expected_resource_version: 0,
                workspace: workspace.to_owned(),
            })
            .await?;
        Ok(())
    }

    async fn update_policy(
        &self,
        sandbox: &str,
        policy: proto::SandboxPolicy,
        workspace: &str,
    ) -> Result<()> {
        // Spelled out rather than `..default()` so a future proto field fails the
        // build here and gets re-checked — the gateway proto is an external
        // contract. The operator only pushes a full policy; the per-setting and
        // merge modes are unused, and annotations/workspace stay at their empty
        // (no-change / "default" workspace) values.
        let request = UpdateConfigRequest {
            name: sandbox.to_owned(),
            policy: Some(policy),
            setting_key: String::new(),
            setting_value: None,
            delete_setting: false,
            global: false,
            merge_operations: Vec::new(),
            // 0 → apply against the current resource version; the operator is the
            // sole writer, matching attach/detach above.
            expected_resource_version: 0,
            annotations: HashMap::new(),
            workspace: workspace.to_owned(),
        };
        match self.grpc().await?.update_config(request).await {
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
        let mut grpc = self.grpc().await?;

        // Fetch any existing provider to decide create-vs-update and carry its
        // metadata (id, resource_version) into the update.
        let existing = match grpc
            .get_provider(GetProviderRequest {
                name: input.name.clone(),
                workspace: input.workspace.clone(),
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
            // Empty = the provider's type profile lives in the platform/global
            // scope. The gateway allows this for any workspace (it only rejects a
            // profile_workspace that is non-empty and mismatched), so global
            // profiles stay valid for a workspaced provider.
            profile_workspace: String::new(),
            // Gateway-owned: handles are minted by its credential storage, and
            // create and update both reject a non-empty map with
            // `InvalidArgument`. Never round-trip them from the fetched
            // provider — the gateway keeps the stored handles either way.
            credential_handles: HashMap::new(),
        };

        if existing.is_some() {
            grpc.update_provider(UpdateProviderRequest {
                provider: Some(provider),
                credential_expires_at_ms: HashMap::new(),
                workspace: input.workspace.clone(),
            })
            .await?;
        } else {
            grpc.create_provider(CreateProviderRequest {
                provider: Some(provider),
                workspace: input.workspace.clone(),
            })
            .await?;
        }
        Ok(())
    }

    async fn delete_provider(&self, name: &str, workspace: &str) -> Result<bool> {
        let mut grpc = self.grpc().await?;
        let response = grpc
            .delete_provider(DeleteProviderRequest {
                name: name.to_owned(),
                workspace: workspace.to_owned(),
            })
            .await?;
        Ok(response.into_inner().deleted)
    }

    async fn list_provider_profiles(&self) -> Result<Vec<ProviderProfileView>> {
        let mut grpc = self.grpc().await?;
        // Spelled out (not `::default()`) so a new proto field fails the build.
        // No paging; empty workspace lists the platform/global profiles.
        let response = grpc
            .list_provider_profiles(proto::ListProviderProfilesRequest {
                limit: 0,
                offset: 0,
                workspace: String::new(),
            })
            .await?;
        Ok(response
            .into_inner()
            .profiles
            .into_iter()
            .map(provider_profile_view)
            .collect())
    }

    async fn configure_provider_refresh(&self, input: ConfigureRefreshInput) -> Result<()> {
        let mut grpc = self.grpc().await?;
        grpc.configure_provider_refresh(proto::ConfigureProviderRefreshRequest {
            provider: input.provider,
            credential_key: input.credential_key,
            strategy: refresh_strategy_to_proto(input.plan.strategy) as i32,
            material: input.plan.material.into_iter().collect(),
            secret_material_keys: input.plan.secret_material_keys,
            // The credential's own expiry is managed by the refresh loop.
            expires_at_ms: None,
            workspace: input.workspace,
        })
        .await?;
        Ok(())
    }

    async fn upsert_provider_profile(&self, mut profile: proto::ProviderProfile) -> Result<u64> {
        let mut grpc = self.grpc().await?;
        let id = profile.id.clone();

        // Fetch any existing profile to decide import-vs-update. A stored custom
        // profile reports a non-zero resource_version; a built-in (or absent)
        // one reports zero, so only a non-zero version counts as "already
        // managed" and eligible for update. Empty workspace = platform scope.
        let existing_version = match grpc
            .get_provider_profile(proto::GetProviderProfileRequest {
                id: id.clone(),
                workspace: String::new(),
            })
            .await
        {
            Ok(response) => response
                .into_inner()
                .profile
                .map(|existing| existing.resource_version)
                .filter(|version| *version > 0),
            Err(status) if status.code() == Code::NotFound => None,
            Err(status) => return Err(status.into()),
        };

        if let Some(version) = existing_version {
            // Carry the current version into the payload too; the request's
            // explicit `expected_resource_version` is authoritative, but the
            // gateway falls back to the embedded one, so keep them consistent.
            profile.resource_version = version;
            let response = grpc
                .update_provider_profiles(proto::UpdateProviderProfilesRequest {
                    profile: Some(import_item(profile)),
                    expected_resource_version: version,
                    id,
                    workspace: String::new(),
                })
                .await?
                .into_inner();
            if !response.updated {
                return Err(Error::ProfileRejected(render_diagnostics(
                    &response.diagnostics,
                )));
            }
            Ok(response
                .profile
                .map_or(version, |updated| updated.resource_version))
        } else {
            let response = grpc
                .import_provider_profiles(proto::ImportProviderProfilesRequest {
                    profiles: vec![import_item(profile)],
                    workspace: String::new(),
                })
                .await?
                .into_inner();
            if !response.imported {
                return Err(Error::ProfileRejected(render_diagnostics(
                    &response.diagnostics,
                )));
            }
            Ok(response
                .profiles
                .into_iter()
                .find(|imported| imported.id == id)
                .map_or(0, |imported| imported.resource_version))
        }
    }

    async fn delete_provider_profile(&self, id: &str) -> Result<bool> {
        let mut grpc = self.grpc().await?;
        match grpc
            .delete_provider_profile(proto::DeleteProviderProfileRequest {
                id: id.to_owned(),
                workspace: String::new(),
            })
            .await
        {
            Ok(response) => Ok(response.into_inner().deleted),
            Err(status) if status.code() == Code::NotFound => Ok(false),
            Err(status) => Err(status.into()),
        }
    }

    async fn create_workspace(&self, create: WorkspaceCreate) -> Result<WorkspaceState> {
        let mut grpc = self.grpc().await?;
        match grpc
            .create_workspace(CreateWorkspaceRequest {
                name: create.name.clone(),
                labels: create.labels.into_iter().collect(),
            })
            .await
        {
            Ok(response) => workspace_state(response.into_inner().workspace),
            // Adopt a workspace that already exists (a create race, or one made
            // out of band): re-read it and return its state rather than erroring.
            Err(status) if status.code() == Code::AlreadyExists => self
                .get_workspace(&create.name)
                .await?
                .ok_or_else(|| missing_workspace(&create.name)),
            Err(status) => Err(status.into()),
        }
    }

    async fn get_workspace(&self, name: &str) -> Result<Option<WorkspaceState>> {
        let mut grpc = self.grpc().await?;
        match grpc
            .get_workspace(GetWorkspaceRequest {
                name: name.to_owned(),
            })
            .await
        {
            Ok(response) => response
                .into_inner()
                .workspace
                .map(|workspace| workspace_state(Some(workspace)))
                .transpose(),
            Err(status) if status.code() == Code::NotFound => Ok(None),
            Err(status) => Err(status.into()),
        }
    }

    async fn delete_workspace(&self, name: &str) -> Result<bool> {
        let mut grpc = self.grpc().await?;
        match grpc
            .delete_workspace(DeleteWorkspaceRequest {
                name: name.to_owned(),
            })
            .await
        {
            Ok(response) => Ok(response.into_inner().deleted),
            Err(status) if status.code() == Code::NotFound => Ok(false),
            Err(status) => Err(status.into()),
        }
    }

    async fn list_workspace_members(&self, workspace: &str) -> Result<Vec<WorkspaceMemberView>> {
        let mut grpc = self.grpc().await?;
        let mut members = Vec::new();
        let mut offset = 0_u32;
        // Page until the gateway returns nothing more, advancing by however many
        // it actually returned. The gateway caps a single page, so a workspace
        // with many members needs more than one request; not assuming it honours
        // `limit` exactly means a server-side clamp can't make us miss members.
        loop {
            let response = grpc
                .list_workspace_members(ListWorkspaceMembersRequest {
                    workspace: workspace.to_owned(),
                    limit: MEMBER_PAGE,
                    offset,
                })
                .await?
                .into_inner();
            let page = response.members.len();
            members.extend(response.members.into_iter().map(member_view));
            if page == 0 {
                break;
            }
            offset = offset.saturating_add(page.try_into().unwrap_or(u32::MAX));
        }
        Ok(members)
    }

    async fn add_workspace_member(
        &self,
        workspace: &str,
        subject: &str,
        role: WorkspaceRole,
    ) -> Result<()> {
        self.grpc()
            .await?
            .add_workspace_member(AddWorkspaceMemberRequest {
                workspace: workspace.to_owned(),
                principal_subject: subject.to_owned(),
                role: workspace_role_to_proto(role) as i32,
            })
            .await?;
        Ok(())
    }

    async fn remove_workspace_member(&self, workspace: &str, subject: &str) -> Result<()> {
        self.grpc()
            .await?
            .remove_workspace_member(RemoveWorkspaceMemberRequest {
                workspace: workspace.to_owned(),
                principal_subject: subject.to_owned(),
            })
            .await?;
        Ok(())
    }
}

/// Members requested per `list_workspace_members` page. The gateway caps a page
/// server-side; this keeps round-trips low without assuming that cap.
const MEMBER_PAGE: u32 = 100;

/// Error for a workspace that vanished between a create-race and its re-read.
fn missing_workspace(name: &str) -> Error {
    Error::Gateway(SdkError::invalid_config(format!(
        "workspace {name} missing from gateway response"
    )))
}

/// Project a gateway `Workspace` onto [`WorkspaceState`], erroring if the
/// response carried no workspace.
fn workspace_state(workspace: Option<proto::datamodel::v1::Workspace>) -> Result<WorkspaceState> {
    let workspace = workspace.ok_or_else(|| {
        Error::Gateway(SdkError::invalid_config(
            "workspace missing from gateway response",
        ))
    })?;
    let phase = match workspace.status.map(|status| status.phase()) {
        Some(proto::datamodel::v1::WorkspacePhase::Active) => WorkspacePhase::Active,
        Some(proto::datamodel::v1::WorkspacePhase::Terminating) => WorkspacePhase::Terminating,
        _ => WorkspacePhase::Unknown,
    };
    Ok(WorkspaceState { phase })
}

/// Project a raw proto `WorkspaceMember` onto [`WorkspaceMemberView`]. An
/// unspecified/unknown role reads as `User` (the least-privileged mapping).
fn member_view(member: proto::WorkspaceMember) -> WorkspaceMemberView {
    let role = match member.role() {
        proto::WorkspaceRole::Admin => WorkspaceRole::Admin,
        _ => WorkspaceRole::User,
    };
    WorkspaceMemberView {
        subject: member.principal_subject,
        role,
    }
}

/// Map a [`WorkspaceRole`] onto its proto discriminant.
fn workspace_role_to_proto(role: WorkspaceRole) -> proto::WorkspaceRole {
    match role {
        WorkspaceRole::User => proto::WorkspaceRole::User,
        WorkspaceRole::Admin => proto::WorkspaceRole::Admin,
    }
}

/// The `source` label attached to profile import/update items. The gateway
/// echoes it in any diagnostics, so it identifies the operator as the writer.
const PROFILE_SOURCE: &str = "openshell-operator";

/// Wrap a profile in the import/update envelope the gateway's profile RPCs take.
fn import_item(profile: proto::ProviderProfile) -> proto::ProviderProfileImportItem {
    proto::ProviderProfileImportItem {
        profile: Some(profile),
        source: PROFILE_SOURCE.to_owned(),
    }
}

/// Join gateway profile diagnostics into one human-readable message for a
/// `Ready=False` condition, in the gateway's own `field: message` shape.
fn render_diagnostics(diagnostics: &[proto::ProviderProfileDiagnostic]) -> String {
    if diagnostics.is_empty() {
        return "gateway rejected the profile without diagnostics".to_owned();
    }
    diagnostics
        .iter()
        .map(|diagnostic| format!("{}: {}", diagnostic.field, diagnostic.message))
        .collect::<Vec<_>>()
        .join("; ")
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
        workspace,
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
            // Spelled out rather than `..default()` so a future proto field
            // fails the build here and gets re-checked. The CRD models neither
            // field: an empty command normalizes to the gateway's portable
            // scratch login shell, and tty stays off.
            command: Vec::new(),
            tty: false,
        }),
        name,
        labels: HashMap::new(),
        annotations: HashMap::new(),
        workspace,
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
    use super::{
        FileTokenRefresher, build_auth, create_sandbox_request, json_value_to_struct, read_file,
    };
    use crate::gateway::SandboxCreate;
    use openshell_sdk::{AuthConfig, Refresh};
    use serde_json::json;

    #[tokio::test]
    async fn file_token_refresher_rereads_rotated_file() {
        // The whole point of the refresher: a token rewritten on disk (a
        // rotated projected token, or a refreshed Secret) is observed on the
        // next refresh without recreating the client.
        let path = std::env::temp_dir().join(format!("openshell-refresh-{}", std::process::id()));
        std::fs::write(&path, "token-one\n").expect("write initial token");
        let refresher = FileTokenRefresher {
            path: path.to_str().unwrap().to_owned(),
        };

        let first = refresher.refresh().await.expect("initial read");
        assert_eq!(first.access_token, "token-one");
        assert!(
            first.expires_at.is_some(),
            "expiry drives proactive refresh"
        );

        std::fs::write(&path, "token-two\n").expect("rotate token");
        let second = refresher.refresh().await.expect("re-read");
        assert_eq!(
            second.access_token, "token-two",
            "picks up the rotated token"
        );

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn file_token_refresher_missing_file_is_transient() {
        // A vanished file (mid-rotation) must be retryable, not terminal, so
        // the SDK backs off and retries instead of failing the connection.
        let refresher = FileTokenRefresher {
            path: "/nonexistent/openshell/token".to_owned(),
        };
        let err = refresher.refresh().await.expect_err("missing file errors");
        assert!(matches!(err, openshell_sdk::RefreshError::Transient(_)));
    }

    #[test]
    fn build_auth_wires_refresher_only_for_file_backed_token() {
        // File-backed token → refresher + expiry (rotates in place).
        match build_auth(Some("tok".to_owned()), Some("/run/token".to_owned())) {
            Some(AuthConfig::Oidc {
                refresh,
                expires_at,
                ..
            }) => {
                assert!(refresh.is_some(), "file-backed token wires a refresher");
                assert!(expires_at.is_some(), "expiry set so refresh triggers");
            }
            _ => panic!("expected an Oidc auth config"),
        }

        // Token with no file → static bearer, no refresher.
        match build_auth(Some("tok".to_owned()), None) {
            Some(AuthConfig::Oidc { refresh, .. }) => {
                assert!(refresh.is_none(), "a fileless token stays static");
            }
            _ => panic!("expected an Oidc auth config"),
        }

        // No token → anonymous.
        assert!(
            build_auth(None, None).is_none(),
            "no token connects anonymously"
        );
    }

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
