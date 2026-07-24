// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconciliation loop for [`OpenShellProvider`].
//!
//! Resolves credentials from the referenced Secret (entitlement-checked),
//! syncs them to the gateway's provider API, and mirrors state to `.status`.
//! Watches referenced Secrets so credential rotation triggers a resync, and
//! uses a finalizer to delete the provider on the gateway when the CR is
//! removed.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    Api, Resource, ResourceExt,
    api::{Patch, PatchParams},
    runtime::{
        Controller,
        controller::Action,
        finalizer::{Event as Finalizer, finalizer},
        reflector::ObjectRef,
        watcher,
    },
};
use tracing::{info, warn};

use super::{Context, ERROR_REQUEUE_INTERVAL, REQUEUE_INTERVAL, record_failure};
use crate::conditions;
use crate::crd::{OpenShellProvider, OpenShellProviderStatus};
use crate::credentials::{self, CredentialMode};
use crate::error::{Error, Result};
use crate::gateway::{ConfigureRefreshInput, Gateway, ProviderInput, ProviderProfileView};
use crate::secret;

/// Finalizer key guaranteeing gateway-side deletion before the CR is removed.
pub const FINALIZER: &str = "openshell.lenshq.io/provider-cleanup";

/// Run the provider controller until the process is stopped.
///
/// Watches referenced Secrets: when a Secret changes, every `OpenShellProvider` in the
/// same namespace that references it is re-queued, so credential rotation
/// propagates to the gateway without polling.
pub async fn run(ctx: Arc<Context>) {
    let providers: Api<OpenShellProvider> = Api::all(ctx.kube.clone());
    let secrets: Api<Secret> = Api::all(ctx.kube.clone());

    let controller = Controller::new(providers, watcher::Config::default());
    let store = controller.store();

    controller
        .watches(secrets, watcher::Config::default(), move |secret| {
            let secret_namespace = secret.namespace();
            let secret_name = secret.name_any();
            store
                .state()
                .into_iter()
                .filter(|provider| {
                    references_secret(provider, secret_namespace.as_deref(), &secret_name)
                })
                .map(|provider| ObjectRef::from_obj(provider.as_ref()))
                .collect::<Vec<_>>()
        })
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((obj, _)) => info!(provider = %obj.name, "reconciled"),
                Err(err) => warn!(error = %err, "provider reconcile loop error"),
            }
        })
        .await;
}

async fn reconcile(provider: Arc<OpenShellProvider>, ctx: Arc<Context>) -> Result<Action> {
    let namespace = provider.namespace().ok_or(Error::MissingNamespace)?;
    let api: Api<OpenShellProvider> = Api::namespaced(ctx.kube.clone(), &namespace);

    finalizer(&api, FINALIZER, provider, |event| async {
        match event {
            Finalizer::Apply(obj) => apply(obj, ctx.clone()).await,
            Finalizer::Cleanup(obj) => cleanup(obj, ctx.clone()).await,
        }
    })
    .await
    .map_err(|err| Error::Finalizer(Box::new(err)))
}

/// Resolve credentials, sync them to the gateway, and record the outcome.
async fn apply(provider: Arc<OpenShellProvider>, ctx: Arc<Context>) -> Result<Action> {
    let name = provider.name_any();
    let namespace = provider.namespace().ok_or(Error::MissingNamespace)?;
    info!(%name, %namespace, "reconciling OpenShellProvider");

    let generation = provider.meta().generation;
    let now = Time(chrono::Utc::now());
    let mut current = provider.status.clone().unwrap_or_default().conditions;

    match sync_provider(&ctx, &provider, &namespace, &name).await {
        Ok(outcome) => {
            conditions::set(
                &mut current,
                conditions::condition(
                    conditions::READY,
                    true,
                    "Reconciled",
                    "credentials synced to the gateway",
                    generation,
                    now,
                ),
            );
            let status = OpenShellProviderStatus {
                conditions: current,
                observed_generation: generation,
                synced_hash: Some(outcome.hash),
                credential_mode: Some(outcome.mode),
            };
            patch_status(&ctx, &namespace, &name, &status).await?;
            Ok(Action::requeue(REQUEUE_INTERVAL))
        }
        Err(err) => {
            record_failure(&ctx, provider.as_ref(), "Sync", &err).await;
            conditions::set(
                &mut current,
                conditions::condition(
                    conditions::READY,
                    false,
                    err.reason(),
                    err.to_string(),
                    generation,
                    now,
                ),
            );
            // Record the failure but keep any prior synced hash + mode for visibility.
            let status = OpenShellProviderStatus {
                conditions: current,
                observed_generation: generation,
                synced_hash: provider.status.as_ref().and_then(|s| s.synced_hash.clone()),
                credential_mode: provider.status.as_ref().and_then(|s| s.credential_mode),
            };
            // Don't let a status-patch failure mask the real sync error.
            if let Err(patch_err) = patch_status(&ctx, &namespace, &name, &status).await {
                warn!(error = %patch_err, "failed to record failure status");
            }
            Err(err)
        }
    }
}

/// Outcome of a successful sync: the material hash and how the credentials were
/// handed to the gateway (for `.status`).
struct SyncOutcome {
    hash: String,
    mode: CredentialMode,
}

/// Resolve the referenced Secret and (idempotently) sync the provider.
async fn sync_provider(
    ctx: &Context,
    provider: &OpenShellProvider,
    namespace: &str,
    name: &str,
) -> Result<SyncOutcome> {
    let credentials =
        secret::resolve_credentials(&ctx.kube, namespace, &provider.spec.credentials_secret_ref)
            .await?;
    sync_to_gateway(
        ctx.gateway.as_ref(),
        name,
        &provider.spec.provider_type,
        credentials,
        provider.spec.config.clone(),
        workspace_of(provider),
    )
    .await
}

/// The gateway workspace a provider targets. Empty (the gateway's `default`)
/// when `spec.workspace` is unset — preserving the pre-workspace behaviour.
fn workspace_of(provider: &OpenShellProvider) -> &str {
    provider.spec.workspace.as_deref().unwrap_or_default()
}

/// Climb the credential ladder and sync the provider to the gateway.
///
/// The provider-type profile decides per-credential handling: values backing a
/// gateway-mintable credential are configured as a refresh (their seed material
/// routed away from the stored credentials), everything else is copied. The
/// provider is upserted first because `ConfigureProviderRefresh` requires it to
/// already exist. Split out from Secret resolution so it is testable against a
/// fake gateway.
async fn sync_to_gateway(
    gateway: &dyn Gateway,
    name: &str,
    provider_type: &str,
    credentials: BTreeMap<String, String>,
    config: BTreeMap<String, String>,
    workspace: &str,
) -> Result<SyncOutcome> {
    let hash = hash_material(&credentials, &config);

    let profile = find_profile(gateway, provider_type).await?;
    let plan = credentials::plan_credentials(
        profile
            .iter()
            .flat_map(|p| p.credentials.iter())
            .map(|c| (c.name.as_str(), c.refresh.as_ref())),
        &credentials,
    );
    let mode = plan.mode();

    // A refresh-capable credential given only partial material is copied as a
    // long-lived secret — the exposure refresh exists to avoid. Warn so it is
    // not silent; the fix is to complete the Secret's material.
    for degraded in &plan.degraded {
        warn!(
            %name,
            credential = %degraded.credential_key,
            missing = ?degraded.missing,
            "refresh material incomplete; credential stored as a static secret \
             instead of gateway-minted — supply the missing keys to avoid it"
        );
    }

    // 1. Ensure the provider exists with its static credentials.
    gateway
        .upsert_provider(ProviderInput {
            name: name.to_owned(),
            provider_type: provider_type.to_owned(),
            credentials: plan.static_credentials,
            config,
            workspace: workspace.to_owned(),
        })
        .await?;

    // 2. Configure gateway-minted refresh for the credentials that support it.
    // Ordered after the upsert because `ConfigureProviderRefresh` requires the
    // provider to exist. If a configure fails here the provider is briefly left
    // without that credential; the reconcile retries and both calls are
    // idempotent, so it converges.
    for refresh in plan.refreshes {
        gateway
            .configure_provider_refresh(ConfigureRefreshInput {
                provider: name.to_owned(),
                credential_key: refresh.credential_key,
                plan: refresh.plan,
                workspace: workspace.to_owned(),
            })
            .await?;
    }

    Ok(SyncOutcome { hash, mode })
}

/// Fetch the profile whose id exactly matches `provider_type`, or `None` when
/// the gateway declares no such profile (an unknown/custom type, handled as a
/// plain static copy). The gateway does not alias types, so the match is exact.
async fn find_profile(
    gateway: &dyn Gateway,
    provider_type: &str,
) -> Result<Option<ProviderProfileView>> {
    Ok(gateway
        .list_provider_profiles()
        .await?
        .into_iter()
        .find(|profile| profile.id == provider_type))
}

/// Delete the provider on the gateway before the finalizer releases the CR.
async fn cleanup(provider: Arc<OpenShellProvider>, ctx: Arc<Context>) -> Result<Action> {
    let name = provider.name_any();
    info!(%name, "deleting provider on gateway");
    if !ctx
        .gateway
        .delete_provider(&name, workspace_of(&provider))
        .await?
    {
        info!(%name, "provider already absent on gateway");
    }
    Ok(Action::await_change())
}

async fn patch_status(
    ctx: &Context,
    namespace: &str,
    name: &str,
    status: &OpenShellProviderStatus,
) -> Result<()> {
    let api: Api<OpenShellProvider> = Api::namespaced(ctx.kube.clone(), namespace);
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

/// Whether `provider` references `secret_name` in `secret_namespace`.
fn references_secret(
    provider: &OpenShellProvider,
    secret_namespace: Option<&str>,
    secret_name: &str,
) -> bool {
    provider.namespace().as_deref() == secret_namespace
        && provider.spec.credentials_secret_ref.name == secret_name
}

/// Deterministic hash of the credential + config material, used to surface
/// rotation in `.status`. `BTreeMap` iteration is sorted, so the hash is stable
/// for identical material.
fn hash_material(
    credentials: &BTreeMap<String, String>,
    config: &BTreeMap<String, String>,
) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (key, value) in credentials {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    // Domain separator between the two maps.
    0xff_u8.hash(&mut hasher);
    for (key, value) in config {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

fn error_policy(_provider: Arc<OpenShellProvider>, err: &Error, _ctx: Arc<Context>) -> Action {
    warn!(error = %err, "provider reconcile failed; requeueing");
    Action::requeue(ERROR_REQUEUE_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::{Gateway, hash_material, references_secret, sync_to_gateway};
    use crate::crd::{OpenShellProvider, OpenShellProviderSpec, SecretRef};
    use crate::credentials::{CredentialMode, MaterialSpec, RefreshSpec, RefreshStrategy};
    use crate::error::Result;
    use crate::gateway::{
        ConfigureRefreshInput, ProviderInput, ProviderProfileCredential, ProviderProfileView,
        WorkspaceCreate, WorkspaceMemberView, WorkspaceRole, WorkspaceState,
    };
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// In-memory `Gateway` recording provider upserts, refresh configuration,
    /// and deletes. `profiles` is what `list_provider_profiles` returns; `order`
    /// records the call sequence so sequencing invariants can be asserted.
    #[derive(Default)]
    struct FakeGateway {
        profiles: Vec<ProviderProfileView>,
        upserted: Mutex<Vec<ProviderInput>>,
        configured: Mutex<Vec<ConfigureRefreshInput>>,
        deleted: Mutex<Vec<String>>,
        order: Mutex<Vec<&'static str>>,
    }

    #[async_trait::async_trait]
    impl Gateway for FakeGateway {
        async fn create_sandbox(
            &self,
            _create: crate::gateway::SandboxCreate,
        ) -> Result<crate::gateway::SandboxState> {
            unreachable!("provider controller does not touch sandboxes")
        }
        async fn get_sandbox(
            &self,
            _name: &str,
            _workspace: &str,
        ) -> Result<Option<crate::gateway::SandboxState>> {
            unreachable!("provider controller does not touch sandboxes")
        }
        async fn delete_sandbox(&self, _name: &str, _workspace: &str) -> Result<bool> {
            unreachable!("provider controller does not touch sandboxes")
        }
        async fn attach_provider(
            &self,
            _sandbox: &str,
            _provider: &str,
            _workspace: &str,
        ) -> Result<()> {
            unreachable!("provider controller does not touch sandboxes")
        }
        async fn detach_provider(
            &self,
            _sandbox: &str,
            _provider: &str,
            _workspace: &str,
        ) -> Result<()> {
            unreachable!("provider controller does not touch sandboxes")
        }
        async fn update_policy(
            &self,
            _sandbox: &str,
            _policy: openshell_sdk::raw::proto::SandboxPolicy,
            _workspace: &str,
        ) -> Result<()> {
            unreachable!("provider controller does not touch sandboxes")
        }
        async fn upsert_provider(&self, input: ProviderInput) -> Result<()> {
            self.order.lock().unwrap().push("upsert");
            self.upserted.lock().unwrap().push(input);
            Ok(())
        }
        async fn delete_provider(&self, name: &str, _workspace: &str) -> Result<bool> {
            self.deleted.lock().unwrap().push(name.to_owned());
            Ok(true)
        }
        async fn list_provider_profiles(&self) -> Result<Vec<ProviderProfileView>> {
            Ok(self.profiles.clone())
        }
        async fn configure_provider_refresh(&self, input: ConfigureRefreshInput) -> Result<()> {
            self.order.lock().unwrap().push("configure");
            self.configured.lock().unwrap().push(input);
            Ok(())
        }
        async fn create_workspace(&self, _create: WorkspaceCreate) -> Result<WorkspaceState> {
            unreachable!("provider controller does not touch workspaces")
        }
        async fn get_workspace(&self, _name: &str) -> Result<Option<WorkspaceState>> {
            unreachable!("provider controller does not touch workspaces")
        }
        async fn delete_workspace(&self, _name: &str) -> Result<bool> {
            unreachable!("provider controller does not touch workspaces")
        }
        async fn list_workspace_members(
            &self,
            _workspace: &str,
        ) -> Result<Vec<WorkspaceMemberView>> {
            unreachable!("provider controller does not touch workspaces")
        }
        async fn add_workspace_member(
            &self,
            _workspace: &str,
            _subject: &str,
            _role: WorkspaceRole,
        ) -> Result<()> {
            unreachable!("provider controller does not touch workspaces")
        }
        async fn remove_workspace_member(&self, _workspace: &str, _subject: &str) -> Result<()> {
            unreachable!("provider controller does not touch workspaces")
        }
    }

    fn provider(namespace: &str, secret_name: &str) -> OpenShellProvider {
        let mut p = OpenShellProvider::new(
            "prov",
            OpenShellProviderSpec {
                provider_type: "claude".to_owned(),
                credentials_secret_ref: SecretRef {
                    name: secret_name.to_owned(),
                    keys: Vec::new(),
                },
                config: BTreeMap::new(),
                workspace: None,
            },
        );
        p.metadata.namespace = Some(namespace.to_owned());
        p
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn matches_referencing_provider_in_same_namespace() {
        let p = provider("team-a", "creds");
        assert!(references_secret(&p, Some("team-a"), "creds"));
    }

    #[test]
    fn ignores_other_namespace_or_name() {
        let p = provider("team-a", "creds");
        assert!(!references_secret(&p, Some("team-b"), "creds"));
        assert!(!references_secret(&p, Some("team-a"), "other"));
    }

    fn oauth2_spec() -> RefreshSpec {
        RefreshSpec {
            strategy: RefreshStrategy::Oauth2RefreshToken,
            material: vec![
                MaterialSpec {
                    name: "client_id".to_owned(),
                    required: true,
                    secret: false,
                },
                MaterialSpec {
                    name: "refresh_token".to_owned(),
                    required: true,
                    secret: true,
                },
            ],
        }
    }

    /// A profile with a single credential named `credential` backed by `refresh`.
    fn profile(id: &str, credential: &str, refresh: RefreshSpec) -> ProviderProfileView {
        ProviderProfileView {
            id: id.to_owned(),
            credentials: vec![ProviderProfileCredential {
                name: credential.to_owned(),
                refresh: Some(refresh),
            }],
        }
    }

    #[tokio::test]
    async fn copies_credentials_for_unknown_type() {
        // No matching profile: every value is copied, matching pre-ladder behaviour.
        let gateway = FakeGateway::default();
        let outcome = sync_to_gateway(
            &gateway,
            "prov",
            "claude",
            map(&[("API_KEY", "sk-123")]),
            map(&[("region", "us")]),
            "",
        )
        .await
        .expect("sync");

        let upserted = gateway.upserted.lock().unwrap();
        assert_eq!(upserted.len(), 1);
        assert_eq!(upserted[0].name, "prov");
        assert_eq!(upserted[0].provider_type, "claude");
        assert_eq!(upserted[0].credentials.get("API_KEY").unwrap(), "sk-123");
        assert!(gateway.configured.lock().unwrap().is_empty());
        assert_eq!(outcome.mode, CredentialMode::Copied);
        assert_eq!(
            outcome.hash,
            hash_material(&map(&[("API_KEY", "sk-123")]), &map(&[("region", "us")]))
        );
    }

    #[tokio::test]
    async fn configures_refresh_for_mintable_credential() {
        let gateway = FakeGateway {
            profiles: vec![profile("vertex", "gcloud_adc_token", oauth2_spec())],
            ..FakeGateway::default()
        };
        let outcome = sync_to_gateway(
            &gateway,
            "prov",
            "vertex",
            map(&[("client_id", "id"), ("refresh_token", "rt")]),
            BTreeMap::new(),
            "",
        )
        .await
        .expect("sync");

        // Seed material is routed to the refresh, never stored as a credential.
        let upserted = gateway.upserted.lock().unwrap();
        assert!(upserted[0].credentials.is_empty());

        let configured = gateway.configured.lock().unwrap();
        assert_eq!(configured.len(), 1);
        assert_eq!(configured[0].provider, "prov");
        assert_eq!(configured[0].credential_key, "gcloud_adc_token");
        assert_eq!(
            configured[0].plan.strategy,
            RefreshStrategy::Oauth2RefreshToken
        );
        assert_eq!(
            configured[0].plan.material.get("refresh_token").unwrap(),
            "rt"
        );

        assert_eq!(outcome.mode, CredentialMode::Refresh);
        // The provider must exist before refresh is configured.
        assert_eq!(*gateway.order.lock().unwrap(), vec!["upsert", "configure"]);
    }

    #[tokio::test]
    async fn mixes_copied_and_refreshed_credentials() {
        // Profile declares a static `api_key` and a mintable `gcloud_adc_token`.
        let gateway = FakeGateway {
            profiles: vec![ProviderProfileView {
                id: "mixed".to_owned(),
                credentials: vec![
                    ProviderProfileCredential {
                        name: "api_key".to_owned(),
                        refresh: None,
                    },
                    ProviderProfileCredential {
                        name: "gcloud_adc_token".to_owned(),
                        refresh: Some(oauth2_spec()),
                    },
                ],
            }],
            ..FakeGateway::default()
        };
        let outcome = sync_to_gateway(
            &gateway,
            "prov",
            "mixed",
            map(&[
                ("api_key", "sk-x"),
                ("client_id", "id"),
                ("refresh_token", "rt"),
            ]),
            BTreeMap::new(),
            "",
        )
        .await
        .expect("sync");

        let upserted = gateway.upserted.lock().unwrap();
        assert_eq!(upserted[0].credentials.get("api_key").unwrap(), "sk-x");
        assert!(!upserted[0].credentials.contains_key("refresh_token"));
        assert_eq!(gateway.configured.lock().unwrap().len(), 1);
        assert_eq!(outcome.mode, CredentialMode::Mixed);
    }

    #[test]
    fn hash_is_stable_and_value_sensitive() {
        let creds = map(&[("KEY", "secret1")]);
        let config = map(&[("region", "us")]);
        assert_eq!(
            hash_material(&creds, &config),
            hash_material(&map(&[("KEY", "secret1")]), &map(&[("region", "us")]))
        );
        // A rotated credential value changes the hash.
        assert_ne!(
            hash_material(&creds, &config),
            hash_material(&map(&[("KEY", "secret2")]), &config)
        );
    }
}
