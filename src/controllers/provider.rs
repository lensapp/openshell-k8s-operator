// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconciliation loop for [`Provider`].
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

use super::{Context, ERROR_REQUEUE_INTERVAL, REQUEUE_INTERVAL};
use crate::crd::{Provider, ProviderPhase, ProviderStatus};
use crate::error::{Error, Result};
use crate::gateway::{Gateway, ProviderInput};
use crate::secret;

/// Finalizer key guaranteeing gateway-side deletion before the CR is removed.
pub const FINALIZER: &str = "openshell.lenshq.io/provider-cleanup";

/// Run the provider controller until the process is stopped.
///
/// Watches referenced Secrets: when a Secret changes, every `Provider` in the
/// same namespace that references it is re-queued, so credential rotation
/// propagates to the gateway without polling.
pub async fn run(ctx: Arc<Context>) {
    let providers: Api<Provider> = Api::all(ctx.kube.clone());
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

async fn reconcile(provider: Arc<Provider>, ctx: Arc<Context>) -> Result<Action> {
    let namespace = provider.namespace().ok_or(Error::MissingNamespace)?;
    let api: Api<Provider> = Api::namespaced(ctx.kube.clone(), &namespace);

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
async fn apply(provider: Arc<Provider>, ctx: Arc<Context>) -> Result<Action> {
    let name = provider.name_any();
    let namespace = provider.namespace().ok_or(Error::MissingNamespace)?;
    info!(%name, %namespace, "reconciling Provider");

    match sync_provider(&ctx, &provider, &namespace, &name).await {
        Ok(hash) => {
            let status = ProviderStatus {
                phase: Some(ProviderPhase::Ready),
                observed_generation: provider.meta().generation,
                synced_hash: Some(hash),
            };
            patch_status(&ctx, &namespace, &name, &status).await?;
            Ok(Action::requeue(REQUEUE_INTERVAL))
        }
        Err(err) => {
            // Record the failure but keep any prior synced hash for visibility.
            let status = ProviderStatus {
                phase: Some(ProviderPhase::Error),
                observed_generation: provider.meta().generation,
                synced_hash: provider.status.as_ref().and_then(|s| s.synced_hash.clone()),
            };
            // Don't let a status-patch failure mask the real sync error.
            if let Err(patch_err) = patch_status(&ctx, &namespace, &name, &status).await {
                warn!(error = %patch_err, "failed to record Error status");
            }
            Err(err)
        }
    }
}

/// Resolve the referenced Secret and (idempotently) upsert the provider.
/// Returns the hash of the synced material.
async fn sync_provider(
    ctx: &Context,
    provider: &Provider,
    namespace: &str,
    name: &str,
) -> Result<String> {
    let credentials =
        secret::resolve_credentials(&ctx.kube, namespace, &provider.spec.credentials_secret_ref)
            .await?;
    sync_to_gateway(
        ctx.gateway.as_ref(),
        name,
        &provider.spec.provider_type,
        credentials,
        provider.spec.config.clone(),
    )
    .await
}

/// Push resolved credentials + config to the gateway and return their hash.
/// Split out from Secret resolution so it is testable against a fake gateway.
async fn sync_to_gateway(
    gateway: &dyn Gateway,
    name: &str,
    provider_type: &str,
    credentials: BTreeMap<String, String>,
    config: BTreeMap<String, String>,
) -> Result<String> {
    let hash = hash_material(&credentials, &config);
    gateway
        .upsert_provider(ProviderInput {
            name: name.to_owned(),
            provider_type: provider_type.to_owned(),
            credentials,
            config,
        })
        .await?;
    Ok(hash)
}

/// Delete the provider on the gateway before the finalizer releases the CR.
async fn cleanup(provider: Arc<Provider>, ctx: Arc<Context>) -> Result<Action> {
    let name = provider.name_any();
    info!(%name, "deleting provider on gateway");
    if !ctx.gateway.delete_provider(&name).await? {
        info!(%name, "provider already absent on gateway");
    }
    Ok(Action::await_change())
}

async fn patch_status(
    ctx: &Context,
    namespace: &str,
    name: &str,
    status: &ProviderStatus,
) -> Result<()> {
    let api: Api<Provider> = Api::namespaced(ctx.kube.clone(), namespace);
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

/// Whether `provider` references `secret_name` in `secret_namespace`.
fn references_secret(
    provider: &Provider,
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

fn error_policy(_provider: Arc<Provider>, err: &Error, _ctx: Arc<Context>) -> Action {
    warn!(error = %err, "provider reconcile failed; requeueing");
    Action::requeue(ERROR_REQUEUE_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::{Gateway, hash_material, references_secret, sync_to_gateway};
    use crate::crd::{Provider, ProviderSpec, SecretRef};
    use crate::error::Result;
    use crate::gateway::ProviderInput;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// In-memory `Gateway` recording provider upserts/deletes.
    #[derive(Default)]
    struct FakeGateway {
        upserted: Mutex<Vec<ProviderInput>>,
        deleted: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Gateway for FakeGateway {
        async fn create_sandbox(
            &self,
            _create: crate::gateway::SandboxCreate,
        ) -> Result<crate::gateway::SandboxState> {
            unreachable!("provider controller does not touch sandboxes")
        }
        async fn get_sandbox(&self, _name: &str) -> Result<Option<crate::gateway::SandboxState>> {
            unreachable!("provider controller does not touch sandboxes")
        }
        async fn delete_sandbox(&self, _name: &str) -> Result<bool> {
            unreachable!("provider controller does not touch sandboxes")
        }
        async fn upsert_provider(&self, input: ProviderInput) -> Result<()> {
            self.upserted.lock().unwrap().push(input);
            Ok(())
        }
        async fn delete_provider(&self, name: &str) -> Result<bool> {
            self.deleted.lock().unwrap().push(name.to_owned());
            Ok(true)
        }
    }

    fn provider(namespace: &str, secret_name: &str) -> Provider {
        let mut p = Provider::new(
            "prov",
            ProviderSpec {
                provider_type: "claude".to_owned(),
                credentials_secret_ref: SecretRef {
                    name: secret_name.to_owned(),
                    keys: Vec::new(),
                },
                config: BTreeMap::new(),
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

    #[tokio::test]
    async fn syncs_resolved_credentials_to_gateway() {
        let gateway = FakeGateway::default();
        let hash = sync_to_gateway(
            &gateway,
            "prov",
            "claude",
            map(&[("API_KEY", "sk-123")]),
            map(&[("region", "us")]),
        )
        .await
        .expect("sync");

        let upserted = gateway.upserted.lock().unwrap();
        assert_eq!(upserted.len(), 1);
        assert_eq!(upserted[0].name, "prov");
        assert_eq!(upserted[0].provider_type, "claude");
        assert_eq!(upserted[0].credentials.get("API_KEY").unwrap(), "sk-123");
        assert_eq!(
            hash,
            hash_material(&map(&[("API_KEY", "sk-123")]), &map(&[("region", "us")]))
        );
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
