// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconciliation loop for [`OpenShellSandbox`].
//!
//! Converges the gateway sandbox to the desired spec, keyed on the resource
//! name: create when absent, reuse when unchanged, and delete+recreate when an
//! immutable field drifts (operator-owned volumes survive and reattach). A
//! finalizer guarantees gateway-side cleanup on delete, and gateway state is
//! mirrored back into `.status`.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    Api, Resource, ResourceExt,
    api::{DeleteParams, ListParams, Patch, PatchParams, PostParams},
    runtime::{
        Controller,
        controller::Action,
        events::EventType,
        finalizer::{Event as Finalizer, finalizer},
        watcher,
    },
};
use openshell_sdk::SandboxPhase;
use openshell_sdk::raw::proto;
use serde_json::json;
use tracing::{info, warn};

use super::{Context, ERROR_REQUEUE_INTERVAL, REQUEUE_INTERVAL, record_event, record_failure};
use crate::crd::{
    OpenShellPolicy, OpenShellPolicySpec, OpenShellSandbox, OpenShellSandboxSpec,
    OpenShellSandboxStatus, Phase, VolumeRetention,
};
use crate::error::{Error, Result};
use crate::gateway::{SandboxCreate, SandboxState};
use crate::{conditions, policy, volumes};

/// How long to wait between polls for the old gateway sandbox to disappear
/// during a recreate, and how many times to poll before giving up this
/// reconcile (and requeueing). The product (~60s) is the worst-case time a
/// recreate blocks its reconcile worker — acceptable because recreate is a rare
/// event; see [`recreate`].
const RECREATE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const RECREATE_POLL_ATTEMPTS: usize = 30;

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
///
/// The `Ready` condition is written on both success and failure, so a failed
/// reconcile (bad volume, policy conflict, gateway error) surfaces on the
/// resource instead of only in the operator log.
async fn apply(sandbox: Arc<OpenShellSandbox>, ctx: Arc<Context>) -> Result<Action> {
    let name = sandbox.name_any();
    let namespace = sandbox.namespace().ok_or(Error::MissingNamespace)?;
    info!(%name, %namespace, "reconciling OpenShellSandbox");

    let generation = sandbox.meta().generation;
    let now = Time(chrono::Utc::now());
    let prior = sandbox.status.clone().unwrap_or_default();
    let mut current = prior.conditions.clone();
    let desired_hash = immutable_fingerprint(&sandbox.spec);

    match converge(&ctx, &namespace, &name, &sandbox, &desired_hash).await {
        Ok(state) => {
            conditions::set(
                &mut current,
                conditions::condition(
                    conditions::READY,
                    true,
                    "Reconciled",
                    "sandbox reconciled with the gateway",
                    generation,
                    now,
                ),
            );
            let status = OpenShellSandboxStatus {
                conditions: current,
                phase: Some(map_phase(state.phase)),
                sandbox_id: Some(state.id),
                applied_spec_hash: Some(desired_hash),
                observed_generation: generation,
            };
            patch_status(&ctx, &namespace, &name, &status).await?;
            Ok(Action::requeue(REQUEUE_INTERVAL))
        }
        Err(err) => {
            record_failure(&ctx, sandbox.as_ref(), "Reconcile", &err).await;
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
            // Keep any prior gateway phase / id / applied hash for visibility.
            let status = OpenShellSandboxStatus {
                conditions: current,
                phase: prior.phase,
                sandbox_id: prior.sandbox_id,
                applied_spec_hash: prior.applied_spec_hash,
                observed_generation: generation,
            };
            if let Err(patch_err) = patch_status(&ctx, &namespace, &name, &status).await {
                warn!(error = %patch_err, "failed to record failure status");
            }
            Err(err)
        }
    }
}

/// Provision volumes, then create / reuse / recreate the gateway sandbox.
///
/// Volumes are provisioned first (idempotent) so their PVCs exist before the
/// gateway schedules the pod that mounts them. Then:
/// - no gateway sandbox yet → create it;
/// - one exists and its applied immutable-spec hash matches → reuse as-is;
/// - one exists but an *immutable* field drifted (image, env, gpu, inline
///   landlock/process, volume mounts) → delete and recreate it, since the
///   gateway forbids changing those on a live sandbox. Operator-owned volumes
///   survive the recreate and reattach by name.
///
/// A sandbox with no recorded hash (created before this operator tracked it) is
/// adopted without recreating. Policy resolution stays on the create/recreate
/// path so re-reconciles of a settled sandbox are cheap and don't fail if a
/// referenced `OpenShellPolicy` is later removed.
async fn converge(
    ctx: &Context,
    namespace: &str,
    name: &str,
    sandbox: &OpenShellSandbox,
    desired_hash: &str,
) -> Result<SandboxState> {
    volumes::validate(&sandbox.spec.volumes)?;
    ensure_pvcs(ctx, namespace, name, &sandbox.spec.volumes).await?;

    let Some(existing) = ctx.gateway.get_sandbox(name).await? else {
        return create(ctx, namespace, name, sandbox).await;
    };

    let applied = sandbox
        .status
        .as_ref()
        .and_then(|status| status.applied_spec_hash.as_deref());
    if !immutable_drift(applied, desired_hash) {
        return Ok(existing);
    }

    warn!(%name, "immutable spec drift; recreating gateway sandbox");
    record_event(
        ctx,
        sandbox,
        EventType::Normal,
        "Recreating",
        "Recreate",
        "immutable field changed; deleting and recreating the gateway sandbox (operator-owned volumes are preserved)".to_owned(),
    )
    .await;
    recreate(ctx, namespace, name, sandbox).await
}

/// Whether an existing gateway sandbox must be recreated to match desired state.
///
/// Recreate only when a hash was previously recorded and differs. A missing
/// hash (`None`) means the sandbox predates hash tracking and is adopted as-is
/// rather than needlessly recreated.
fn immutable_drift(applied: Option<&str>, desired: &str) -> bool {
    matches!(applied, Some(hash) if hash != desired)
}

/// Delete the gateway sandbox, wait for it to disappear, then create it afresh.
///
/// The poll deliberately blocks this reconcile worker for up to ~60s rather
/// than threading a delete/create state machine across reconciles — recreate is
/// a rare event and other resources reconcile on their own worker futures. The
/// gateway's `delete_sandbox` is idempotent, so a `RecreateTimeout` requeue that
/// re-enters this path and deletes again is harmless.
async fn recreate(
    ctx: &Context,
    namespace: &str,
    name: &str,
    sandbox: &OpenShellSandbox,
) -> Result<SandboxState> {
    ctx.gateway.delete_sandbox(name).await?;
    for _ in 0..RECREATE_POLL_ATTEMPTS {
        if ctx.gateway.get_sandbox(name).await?.is_none() {
            return create(ctx, namespace, name, sandbox).await;
        }
        tokio::time::sleep(RECREATE_POLL_INTERVAL).await;
    }
    // Still terminating; bail out and let the requeue retry the create.
    Err(Error::RecreateTimeout {
        name: name.to_owned(),
    })
}

/// Resolve the policy and create the gateway sandbox from the current spec.
async fn create(
    ctx: &Context,
    namespace: &str,
    name: &str,
    sandbox: &OpenShellSandbox,
) -> Result<SandboxState> {
    info!(%name, "creating sandbox on gateway");
    let policy = resolve_policy(ctx, namespace, &sandbox.spec).await?;
    let create = build_sandbox_create(name, &sandbox.spec, policy);
    ctx.gateway.create_sandbox(create).await
}

/// Fingerprint of the spec fields that are immutable on a live gateway sandbox.
///
/// A change here forces a delete+recreate. It deliberately covers only the
/// immutable inputs: `image`, `environment`, `gpu`, the inline policy's
/// `landlock`/`process` (the hard-immutable sections), and the volume mounts
/// (wired into the pod at create time). It excludes `networkPolicies` and
/// `filesystem` (mutable / additively mutable on a live sandbox) and
/// `policyRef` (editing a shared policy is deliberately not retroactive).
fn immutable_fingerprint(spec: &OpenShellSandboxSpec) -> String {
    let inline = spec.policy.as_ref();
    let material = json!({
        "image": spec.image,
        "environment": spec.environment,
        "gpu": spec.gpu,
        "landlock": inline.and_then(|policy| policy.landlock.as_ref()),
        "process": inline.and_then(|policy| policy.process.as_ref()),
        "volumes": spec.volumes.iter().map(|volume| json!({
            "name": volume.name,
            "mountPath": volume.mount_path,
            "subPath": volume.sub_path,
            "readOnly": volume.read_only,
        })).collect::<Vec<_>>(),
    });
    // `serde_json::Map` is BTreeMap-backed, so key order is stable and the
    // rendering is deterministic for a given spec.
    let rendered = serde_json::to_string(&material).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&rendered, &mut hasher);
    format!("{:016x}", std::hash::Hasher::finish(&hasher))
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
    // A terminal error (malformed spec) won't clear until the spec is edited,
    // which re-triggers reconcile on its own — so back off to the normal cadence
    // instead of hot-looping (and re-emitting the same event) every 15s.
    let interval = if err.is_terminal() {
        REQUEUE_INTERVAL
    } else {
        ERROR_REQUEUE_INTERVAL
    };
    Action::requeue(interval)
}

#[cfg(test)]
mod tests {
    use super::{
        Phase, PolicySource, build_sandbox_create, immutable_drift, immutable_fingerprint,
        map_phase, select_policy_source,
    };
    use crate::crd::{
        FilesystemPolicy, LandlockPolicy, OpenShellPolicySpec, OpenShellSandboxSpec,
        PreservedValue, ProcessPolicy, SandboxVolume,
    };
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

    #[test]
    fn immutable_drift_adopts_when_no_prior_hash() {
        // No recorded hash: adopt the existing sandbox, never recreate.
        assert!(!immutable_drift(None, "abc"));
    }

    #[test]
    fn immutable_drift_only_on_changed_hash() {
        assert!(!immutable_drift(Some("abc"), "abc"));
        assert!(immutable_drift(Some("abc"), "def"));
    }

    #[test]
    fn fingerprint_is_stable_for_equal_specs() {
        assert_eq!(
            immutable_fingerprint(&sample_spec()),
            immutable_fingerprint(&sample_spec())
        );
    }

    #[test]
    fn fingerprint_changes_on_immutable_fields() {
        let base = immutable_fingerprint(&sample_spec());

        let mut image = sample_spec();
        image.image = Some("ghcr.io/example/sandbox:v2".to_owned());
        assert_ne!(immutable_fingerprint(&image), base);

        let mut gpu = sample_spec();
        gpu.gpu = false;
        assert_ne!(immutable_fingerprint(&gpu), base);

        let mut env = sample_spec();
        env.environment.insert("LOG".to_owned(), "debug".to_owned());
        assert_ne!(immutable_fingerprint(&env), base);

        let mut landlock = sample_spec();
        landlock.policy = Some(OpenShellPolicySpec {
            landlock: Some(LandlockPolicy {
                compatibility: "enforce".to_owned(),
            }),
            ..OpenShellPolicySpec::default()
        });
        assert_ne!(immutable_fingerprint(&landlock), base);

        let mut process = sample_spec();
        process.policy = Some(OpenShellPolicySpec {
            process: Some(ProcessPolicy {
                run_as_user: "nobody".to_owned(),
                run_as_group: String::new(),
            }),
            ..OpenShellPolicySpec::default()
        });
        assert_ne!(immutable_fingerprint(&process), base);

        let mut volume = sample_spec();
        volume.volumes.push(SandboxVolume {
            name: "data".to_owned(),
            mount_path: "/data".to_owned(),
            sub_path: None,
            read_only: false,
            claim: PersistentVolumeClaimSpec::default(),
        });
        assert_ne!(immutable_fingerprint(&volume), base);
    }

    #[test]
    fn fingerprint_ignores_mutable_fields_and_policy_ref() {
        let base = immutable_fingerprint(&sample_spec());

        // Additively-mutable / mutable sections must not force a recreate.
        let mut filesystem = sample_spec();
        filesystem.policy = Some(OpenShellPolicySpec {
            filesystem: Some(FilesystemPolicy {
                include_workdir: true,
                read_only: vec!["/etc".to_owned()],
                read_write: Vec::new(),
            }),
            ..OpenShellPolicySpec::default()
        });
        assert_eq!(immutable_fingerprint(&filesystem), base);

        let mut network = sample_spec();
        network.policy = Some(OpenShellPolicySpec {
            network_policies: std::iter::once((
                "claude_code".to_owned(),
                PreservedValue(serde_json::json!({ "endpoints": [] })),
            ))
            .collect(),
            ..OpenShellPolicySpec::default()
        });
        assert_eq!(immutable_fingerprint(&network), base);

        // Editing a shared policy by reference is deliberately not retroactive.
        let mut by_ref = sample_spec();
        by_ref.policy_ref = Some("restricted".to_owned());
        assert_eq!(immutable_fingerprint(&by_ref), base);
    }
}
