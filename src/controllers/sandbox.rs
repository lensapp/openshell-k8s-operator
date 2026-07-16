// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconciliation loop for [`OpenShellSandbox`].
//!
//! Idempotent get-or-create against the gateway keyed on the resource name,
//! with a finalizer guaranteeing gateway-side cleanup on delete. Gateway state
//! is mirrored back into `.status`.

use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kube::{
    Api, Resource, ResourceExt,
    api::{DeleteParams, ListParams, Patch, PatchParams, PostParams},
    runtime::{
        Controller,
        controller::Action,
        finalizer::{Event as Finalizer, finalizer},
        watcher,
    },
};
use openshell_sdk::SandboxPhase;
use openshell_sdk::raw::proto;
use serde_json::json;
use tracing::{info, warn};

use super::{Context, ERROR_REQUEUE_INTERVAL, REQUEUE_INTERVAL};
use crate::crd::{
    OpenShellPolicy, OpenShellPolicySpec, OpenShellSandbox, OpenShellSandboxSpec,
    OpenShellSandboxStatus, Phase, VolumeRetention,
};
use crate::error::{Error, Result};
use crate::gateway::SandboxCreate;
use crate::{policy, volumes};

/// Finalizer key guaranteeing gateway-side deletion before the CR is removed.
pub const FINALIZER: &str = "openshell.lenshq.io/sandbox-cleanup";

/// Run the sandbox controller until the process is stopped.
pub async fn run(ctx: Arc<Context>) {
    let sandboxes: Api<OpenShellSandbox> = Api::all(ctx.kube.clone());

    Controller::new(sandboxes, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((obj, _)) => info!(sandbox = %obj.name, "reconciled"),
                Err(err) => warn!(error = %err, "sandbox reconcile loop error"),
            }
        })
        .await;
}

async fn reconcile(sandbox: Arc<OpenShellSandbox>, ctx: Arc<Context>) -> Result<Action> {
    let namespace = sandbox.namespace().ok_or(Error::MissingNamespace)?;
    let api: Api<OpenShellSandbox> = Api::namespaced(ctx.kube.clone(), &namespace);

    finalizer(&api, FINALIZER, sandbox, |event| async {
        match event {
            Finalizer::Apply(obj) => apply(obj, ctx.clone()).await,
            Finalizer::Cleanup(obj) => cleanup(obj, ctx.clone()).await,
        }
    })
    .await
    .map_err(|err| Error::Finalizer(Box::new(err)))
}

/// Create-or-sync the sandbox on the gateway and mirror its state to `.status`.
async fn apply(sandbox: Arc<OpenShellSandbox>, ctx: Arc<Context>) -> Result<Action> {
    let name = sandbox.name_any();
    let namespace = sandbox.namespace().ok_or(Error::MissingNamespace)?;
    info!(%name, %namespace, "reconciling OpenShellSandbox");

    // Provision the sandbox's volumes first (idempotent), so their PVCs exist
    // before the gateway schedules the pod that mounts them.
    volumes::validate(&sandbox.spec.volumes)?;
    ensure_pvcs(&ctx, &namespace, &name, &sandbox.spec.volumes).await?;

    // Reuse the gateway sandbox if it already exists (keyed on the CR name);
    // otherwise resolve the policy and create it. Resolving the policy only on
    // the create path keeps re-reconciles of a running sandbox cheap and avoids
    // spuriously failing it if the referenced OpenShellPolicy is later removed — the
    // policy is immutable on a running sandbox anyway.
    let state = if let Some(existing) = ctx.gateway.get_sandbox(&name).await? {
        existing
    } else {
        info!(%name, "creating sandbox on gateway");
        let policy = resolve_policy(&ctx, &namespace, &sandbox.spec).await?;
        let create = build_sandbox_create(&name, &sandbox.spec, policy);
        ctx.gateway.create_sandbox(create).await?
    };

    let status = OpenShellSandboxStatus {
        phase: Some(map_phase(state.phase)),
        sandbox_id: Some(state.id),
        observed_generation: sandbox.meta().generation,
    };
    patch_status(&ctx, &namespace, &name, &status).await?;

    Ok(Action::requeue(REQUEUE_INTERVAL))
}

/// Where a sandbox's policy document comes from, after enforcing that `policy`
/// and `policyRef` are mutually exclusive.
enum PolicySource<'a> {
    /// No policy set; the gateway applies its default.
    None,
    /// Inline document under `spec.policy`.
    Inline(&'a OpenShellPolicySpec),
    /// Name of an `OpenShellPolicy` to resolve in the sandbox's namespace.
    Ref(&'a str),
}

/// Decide the policy source from the spec, rejecting the illegal "both set"
/// case. Pure and total — the reconcile's only guard until the milestone-4
/// admission webhook rejects the conflict at write time.
fn select_policy_source(spec: &OpenShellSandboxSpec) -> Result<PolicySource<'_>> {
    match (&spec.policy, &spec.policy_ref) {
        (Some(_), Some(_)) => Err(Error::PolicySourceConflict),
        (Some(inline), None) => Ok(PolicySource::Inline(inline)),
        (None, Some(policy_ref)) => Ok(PolicySource::Ref(policy_ref)),
        (None, None) => Ok(PolicySource::None),
    }
}

/// Resolve the sandbox's policy (if any) to a validated proto policy.
///
/// The document may be given inline (`spec.policy`) or by reference
/// (`spec.policyRef`, naming an `OpenShellPolicy` in the sandbox's namespace),
/// but not both. Either source is run through the gateway's parser, so an
/// invalid document fails the sandbox reconcile with a clear error rather than
/// being silently dropped.
async fn resolve_policy(
    ctx: &Context,
    namespace: &str,
    spec: &OpenShellSandboxSpec,
) -> Result<Option<proto::SandboxPolicy>> {
    match select_policy_source(spec)? {
        PolicySource::None => Ok(None),
        PolicySource::Inline(inline) => Ok(Some(policy::to_proto(inline)?)),
        PolicySource::Ref(policy_ref) => {
            let api: Api<OpenShellPolicy> = Api::namespaced(ctx.kube.clone(), namespace);
            let policy = api
                .get_opt(policy_ref)
                .await?
                .ok_or_else(|| Error::PolicyNotFound {
                    namespace: namespace.to_owned(),
                    name: policy_ref.to_owned(),
                })?;
            Ok(Some(policy::to_proto(&policy.spec)?))
        }
    }
}

/// Translate CR spec fields (plus the resolved policy) into the gateway create.
fn build_sandbox_create(
    name: &str,
    spec: &OpenShellSandboxSpec,
    policy: Option<proto::SandboxPolicy>,
) -> SandboxCreate {
    SandboxCreate {
        name: name.to_owned(),
        image: spec.image.clone(),
        environment: spec.environment.clone(),
        providers: spec.providers.clone(),
        gpu: spec.gpu,
        policy,
        driver_config: volumes::driver_config_json(name, &spec.volumes),
    }
}

/// Create the PVC for each volume that does not yet exist. Existing PVCs are
/// left untouched — their spec is largely immutable and their data must be
/// preserved — so this is a safe get-or-create on every reconcile.
async fn ensure_pvcs(
    ctx: &Context,
    namespace: &str,
    name: &str,
    volumes: &[crate::crd::SandboxVolume],
) -> Result<()> {
    if volumes.is_empty() {
        return Ok(());
    }
    let api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.kube.clone(), namespace);
    for volume in volumes {
        let pvc = volumes::build_pvc(name, volume);
        let pvc_name = volumes::pvc_name(name, volume);
        if api.get_opt(&pvc_name).await?.is_none() {
            info!(%name, %pvc_name, "provisioning sandbox volume");
            api.create(&PostParams::default(), &pvc).await?;
        }
    }
    Ok(())
}

/// Delete the sandbox on the gateway before the finalizer releases the CR, and
/// its provisioned volumes when `volumeRetention` is `Delete`.
async fn cleanup(sandbox: Arc<OpenShellSandbox>, ctx: Arc<Context>) -> Result<Action> {
    let name = sandbox.name_any();
    info!(%name, "deleting sandbox on gateway");
    if !ctx.gateway.delete_sandbox(&name).await? {
        info!(%name, "sandbox already absent on gateway");
    }

    if sandbox.spec.volume_retention == VolumeRetention::Delete {
        let namespace = sandbox.namespace().ok_or(Error::MissingNamespace)?;
        info!(%name, "deleting provisioned sandbox volumes");
        let api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.kube.clone(), &namespace);
        api.delete_collection(
            &DeleteParams::default(),
            &ListParams::default().labels(&volumes::selector(&name)),
        )
        .await?;
    }

    Ok(Action::await_change())
}

/// Merge-patch `.status`. Merge (not apply) keeps this free of field-manager
/// ceremony while remaining idempotent for these fields.
async fn patch_status(
    ctx: &Context,
    namespace: &str,
    name: &str,
    status: &OpenShellSandboxStatus,
) -> Result<()> {
    let api: Api<OpenShellSandbox> = Api::namespaced(ctx.kube.clone(), namespace);
    let patch = json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

/// Map the gateway's phase onto the CR's coarse phase.
fn map_phase(phase: SandboxPhase) -> Phase {
    match phase {
        SandboxPhase::Ready => Phase::Ready,
        SandboxPhase::Error => Phase::Error,
        SandboxPhase::Deleting => Phase::Deleting,
        // Provisioning, Unspecified, Unknown, and any future variant read as
        // still-settling.
        _ => Phase::Provisioning,
    }
}

fn error_policy(_sandbox: Arc<OpenShellSandbox>, err: &Error, _ctx: Arc<Context>) -> Action {
    warn!(error = %err, "sandbox reconcile failed; requeueing");
    Action::requeue(ERROR_REQUEUE_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::{Phase, PolicySource, build_sandbox_create, map_phase, select_policy_source};
    use crate::crd::{OpenShellPolicySpec, OpenShellSandboxSpec, SandboxVolume};
    use crate::error::Error;
    use k8s_openapi::api::core::v1::PersistentVolumeClaimSpec;
    use openshell_sdk::SandboxPhase;
    use openshell_sdk::raw::proto;

    fn sample_spec() -> OpenShellSandboxSpec {
        OpenShellSandboxSpec {
            image: Some("ghcr.io/example/sandbox:latest".to_owned()),
            providers: vec!["openai".to_owned()],
            gpu: true,
            ..OpenShellSandboxSpec::default()
        }
    }

    #[test]
    fn build_sandbox_create_maps_cr_fields() {
        let create = build_sandbox_create("named", &sample_spec(), None);
        assert_eq!(create.name, "named");
        assert_eq!(
            create.image.as_deref(),
            Some("ghcr.io/example/sandbox:latest")
        );
        assert_eq!(create.providers, vec!["openai".to_owned()]);
        assert!(create.gpu);
        assert!(create.policy.is_none());
        assert!(create.driver_config.is_none());
    }

    #[test]
    fn build_sandbox_create_carries_volume_driver_config() {
        let spec = OpenShellSandboxSpec {
            volumes: vec![SandboxVolume {
                name: "data".to_owned(),
                mount_path: "/data".to_owned(),
                sub_path: None,
                read_only: false,
                claim: PersistentVolumeClaimSpec::default(),
            }],
            ..OpenShellSandboxSpec::default()
        };
        let create = build_sandbox_create("named", &spec, None);
        let config = create.driver_config.expect("driver_config present");
        assert_eq!(
            config["kubernetes"]["volumes"][0]["persistent_volume_claim"]["claim_name"],
            "named-data"
        );
    }

    #[test]
    fn build_sandbox_create_carries_resolved_policy() {
        let policy = proto::SandboxPolicy {
            version: 1,
            ..proto::SandboxPolicy::default()
        };
        let create = build_sandbox_create("named", &sample_spec(), Some(policy));
        assert_eq!(create.policy.expect("policy present").version, 1);
    }

    #[test]
    fn policy_source_rejects_both_inline_and_ref() {
        let spec = OpenShellSandboxSpec {
            policy: Some(OpenShellPolicySpec::default()),
            policy_ref: Some("restricted".to_owned()),
            ..OpenShellSandboxSpec::default()
        };
        assert!(matches!(
            select_policy_source(&spec),
            Err(Error::PolicySourceConflict)
        ));
    }

    #[test]
    fn policy_source_picks_inline_ref_or_none() {
        let inline = OpenShellSandboxSpec {
            policy: Some(OpenShellPolicySpec::default()),
            ..OpenShellSandboxSpec::default()
        };
        assert!(matches!(
            select_policy_source(&inline),
            Ok(PolicySource::Inline(_))
        ));

        let by_ref = OpenShellSandboxSpec {
            policy_ref: Some("restricted".to_owned()),
            ..OpenShellSandboxSpec::default()
        };
        assert!(matches!(
            select_policy_source(&by_ref),
            Ok(PolicySource::Ref("restricted"))
        ));

        let none = OpenShellSandboxSpec::default();
        assert!(matches!(
            select_policy_source(&none),
            Ok(PolicySource::None)
        ));
    }

    #[test]
    fn maps_gateway_phase_to_cr_phase() {
        assert_eq!(map_phase(SandboxPhase::Ready), Phase::Ready);
        assert_eq!(map_phase(SandboxPhase::Error), Phase::Error);
        assert_eq!(map_phase(SandboxPhase::Deleting), Phase::Deleting);
        assert_eq!(map_phase(SandboxPhase::Provisioning), Phase::Provisioning);
        // Unspecified/unknown settle as provisioning.
        assert_eq!(map_phase(SandboxPhase::Unspecified), Phase::Provisioning);
    }
}
