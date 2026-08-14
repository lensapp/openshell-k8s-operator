// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconciliation loop for [`OpenShellSandbox`].
//!
//! Converges the gateway sandbox to the desired spec, keyed on the resource
//! name: create when absent; converge in place when only mutable fields drift
//! (attach/detach providers, apply the policy's `networkPolicies` and additive
//! `filesystem` via `UpdateConfig`); and delete+recreate when an immutable field
//! drifts (image, env, gpu, volume mounts, or the resolved policy's
//! `landlock`/`process`), with operator-owned volumes surviving and reattaching.
//! A finalizer guarantees gateway-side cleanup on delete, and gateway state is
//! mirrored back into `.status`.

use std::collections::BTreeSet;
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
        reflector::ObjectRef,
        watcher,
    },
};
use openshell_sdk::SandboxPhase;
use openshell_sdk::raw::proto;
use serde_json::json;
use tracing::{info, warn};

use super::{
    Context, ERROR_REQUEUE_INTERVAL, REQUEUE_INTERVAL, TRANSITIONAL_REQUEUE_INTERVAL, record_event,
    record_failure,
};
use crate::crd::{
    OpenShellPolicy, OpenShellPolicySpec, OpenShellSandbox, OpenShellSandboxSpec,
    OpenShellSandboxStatus, Phase, ResourceQuantities, SandboxResources, VolumeRetention,
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
///
/// Watches `OpenShellPolicy`: when a policy changes, every `OpenShellSandbox` in
/// the same namespace that references it via `policyRef` is re-queued, so a
/// shared policy edit converges (in place for its mutable fields, by recreate
/// for its immutable ones) without waiting for the periodic resync.
pub async fn run(ctx: Arc<Context>) {
    let sandboxes: Api<OpenShellSandbox> = Api::all(ctx.kube.clone());
    let policies: Api<OpenShellPolicy> = Api::all(ctx.kube.clone());

    let controller = Controller::new(sandboxes, watcher::Config::default());
    let store = controller.store();

    controller
        .watches(policies, watcher::Config::default(), move |policy| {
            let policy_namespace = policy.namespace();
            let policy_name = policy.name_any();
            store
                .state()
                .into_iter()
                .filter(|sandbox| {
                    references_policy(sandbox, policy_namespace.as_deref(), &policy_name)
                })
                .map(|sandbox| ObjectRef::from_obj(sandbox.as_ref()))
                .collect::<Vec<_>>()
        })
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((obj, _)) => info!(sandbox = %obj.name, "reconciled"),
                Err(err) => warn!(error = %err, "sandbox reconcile loop error"),
            }
        })
        .await;
}

/// The gateway workspace a sandbox targets. Empty (the gateway's `default`)
/// when `spec.workspace` is unset — preserving the pre-workspace behaviour.
///
/// The workspace is part of a sandbox's gateway identity, so it is threaded into
/// every gateway call. It is *not* folded into the immutable fingerprint: it is
/// immutable (a change would orphan the old `{workspace}--{name}` object rather
/// than move it), and leaving the fingerprint untouched keeps every existing
/// sandbox's recorded hash byte-identical across the operator upgrade that adds
/// this field.
fn workspace_of(spec: &OpenShellSandboxSpec) -> &str {
    spec.workspace.as_deref().unwrap_or_default()
}

/// Whether `sandbox` resolves its policy from `policy_name` in `policy_namespace`.
fn references_policy(
    sandbox: &OpenShellSandbox,
    policy_namespace: Option<&str>,
    policy_name: &str,
) -> bool {
    sandbox.namespace().as_deref() == policy_namespace
        && sandbox.spec.policy_ref.as_deref() == Some(policy_name)
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

    match converge(&ctx, &namespace, &name, &sandbox).await {
        Ok(converged) => {
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
            let phase = map_phase(converged.state.phase);
            let status = OpenShellSandboxStatus {
                conditions: current,
                phase: Some(phase),
                sandbox_id: Some(converged.state.id),
                applied_spec_hash: Some(converged.applied_spec_hash),
                applied_policy_hash: Some(converged.applied_policy_hash),
                observed_generation: generation,
            };
            patch_status(&ctx, &namespace, &name, &status).await?;
            Ok(Action::requeue(success_requeue(phase)))
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
            // Keep any prior gateway phase / id / applied hashes for visibility.
            let status = OpenShellSandboxStatus {
                conditions: current,
                phase: prior.phase,
                sandbox_id: prior.sandbox_id,
                applied_spec_hash: prior.applied_spec_hash,
                applied_policy_hash: prior.applied_policy_hash,
                observed_generation: generation,
            };
            if let Err(patch_err) = patch_status(&ctx, &namespace, &name, &status).await {
                warn!(error = %patch_err, "failed to record failure status");
            }
            Err(err)
        }
    }
}

/// Outcome of a successful [`converge`]: the live gateway state plus the hashes
/// to record so the next reconcile can detect drift.
struct Converged {
    /// Live gateway sandbox state, mirrored to `.status`.
    state: SandboxState,
    /// Hash of the immutable spec fields now applied.
    applied_spec_hash: String,
    /// Hash of the mutable policy fields now applied.
    applied_policy_hash: String,
}

/// Provision volumes, then create / reuse / recreate / update the gateway
/// sandbox to match the desired spec (including its resolved policy).
///
/// Volumes are provisioned first (idempotent) so their PVCs exist before the
/// gateway schedules the pod that mounts them. Then, keyed on the resolved
/// policy (inline `spec.policy` or referenced `spec.policyRef`):
/// - no gateway sandbox yet → create it;
/// - an *immutable* field drifted (image, env, gpu, volume mounts, or the
///   resolved policy's `landlock`/`process`) → delete and recreate it, since the
///   gateway forbids changing those on a live sandbox. Operator-owned volumes
///   survive and reattach by name;
/// - otherwise → converge in place: attach/detach providers, and apply the
///   policy's mutable fields (`networkPolicies`, additive `filesystem`) via the
///   gateway's `UpdateConfig` when they drift.
///
/// Drift is measured against the hashes recorded in `.status`, not against live
/// gateway state, because the gateway enriches a policy after create (so a live
/// readback never equals what was sent). A sandbox with no recorded hash is
/// adopted without acting.
///
/// A referenced policy that is missing or invalid is never allowed to tear down
/// a *running* sandbox: providers still converge and the sandbox keeps its
/// last-applied policy, but the resolution error propagates so `Ready` goes
/// `False` until the policy is fixed (rather than falsely reporting success).
///
/// Removing a policy's mutable-only sections (`networkPolicies`/`filesystem`)
/// from a running sandbox is a no-op the gateway cannot strip in place; recreate
/// the sandbox to clear them. (Removing `landlock`/`process` changes the
/// immutable hash and so recreates automatically.)
async fn converge(
    ctx: &Context,
    namespace: &str,
    name: &str,
    sandbox: &OpenShellSandbox,
) -> Result<Converged> {
    volumes::validate(&sandbox.spec.volumes)?;
    ensure_pvcs(ctx, namespace, name, &sandbox.spec.volumes).await?;

    let prior_spec_hash = sandbox
        .status
        .as_ref()
        .and_then(|status| status.applied_spec_hash.clone());
    let prior_policy_hash = sandbox
        .status
        .as_ref()
        .and_then(|status| status.applied_policy_hash.clone());

    let workspace = workspace_of(&sandbox.spec);
    let existing = ctx.gateway.get_sandbox(name, workspace).await?;
    let resolved = resolve(ctx, namespace, &sandbox.spec).await;

    let Some(existing) = existing else {
        // No sandbox yet: resolution errors are fatal — we cannot create the
        // sandbox without a valid policy.
        return create_converged(ctx, name, sandbox, resolved?).await;
    };

    // A missing/invalid policy must not tear down a running sandbox, but it is
    // not healthy either. Converge what we safely can (providers), leave the
    // last-applied policy in force, and propagate the error so `Ready` reflects
    // the unresolved policy until it is fixed.
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(err) => {
            warn!(%name, error = %err, "policy unresolved; keeping running sandbox");
            converge_providers(ctx, name, &sandbox.spec.providers, &existing, workspace).await?;
            return Err(err);
        }
    };

    let desired_spec_hash = immutable_fingerprint(&sandbox.spec, resolved.spec.as_ref());
    if hash_drifted(prior_spec_hash.as_deref(), &desired_spec_hash) {
        return recreate_converged(ctx, name, sandbox, resolved, desired_spec_hash).await;
    }

    // In-place convergence: providers first, then the policy's mutable fields.
    converge_providers(ctx, name, &sandbox.spec.providers, &existing, workspace).await?;

    let desired_policy_hash = mutable_policy_fingerprint(resolved.spec.as_ref());
    update_policy_if_drifted(
        ctx,
        name,
        sandbox,
        resolved.proto,
        prior_policy_hash.as_deref(),
        &desired_policy_hash,
    )
    .await?;

    Ok(Converged {
        state: existing,
        applied_spec_hash: desired_spec_hash,
        applied_policy_hash: desired_policy_hash,
    })
}

/// Create a not-yet-existing gateway sandbox and package the hashes to record.
async fn create_converged(
    ctx: &Context,
    name: &str,
    sandbox: &OpenShellSandbox,
    resolved: ResolvedPolicy,
) -> Result<Converged> {
    let applied_spec_hash = immutable_fingerprint(&sandbox.spec, resolved.spec.as_ref());
    let applied_policy_hash = mutable_policy_fingerprint(resolved.spec.as_ref());
    let state = create(ctx, name, sandbox, resolved.proto).await?;
    Ok(Converged {
        state,
        applied_spec_hash,
        applied_policy_hash,
    })
}

/// Recreate a sandbox whose immutable fields drifted, announce it, and package
/// the hashes to record. Operator-owned volumes survive the recreate.
async fn recreate_converged(
    ctx: &Context,
    name: &str,
    sandbox: &OpenShellSandbox,
    resolved: ResolvedPolicy,
    desired_spec_hash: String,
) -> Result<Converged> {
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
    let applied_policy_hash = mutable_policy_fingerprint(resolved.spec.as_ref());
    let state = recreate(ctx, name, sandbox, resolved.proto).await?;
    Ok(Converged {
        state,
        applied_spec_hash: desired_spec_hash,
        applied_policy_hash,
    })
}

/// Push the policy's mutable fields (`networkPolicies`, additive `filesystem`)
/// to the live sandbox in place when they drift.
async fn update_policy_if_drifted(
    ctx: &Context,
    name: &str,
    sandbox: &OpenShellSandbox,
    policy: Option<proto::SandboxPolicy>,
    prior_policy_hash: Option<&str>,
    desired_policy_hash: &str,
) -> Result<()> {
    if let Some(policy) = policy
        && hash_drifted(prior_policy_hash, desired_policy_hash)
    {
        info!(%name, "policy mutable fields drifted; updating in place");
        record_event(
            ctx,
            sandbox,
            EventType::Normal,
            "PolicyUpdated",
            "UpdatePolicy",
            "applying policy mutable fields (networkPolicies, additive filesystem) in place"
                .to_owned(),
        )
        .await;
        ctx.gateway
            .update_policy(name, policy, workspace_of(&sandbox.spec))
            .await?;
    }
    Ok(())
}

/// Converge the sandbox's attached providers toward `desired`, attaching those
/// missing and detaching those no longer wanted.
///
/// Diffs against the providers the gateway actually reports (`existing`), so it
/// also heals drift applied out of band. Attaching a provider the gateway
/// doesn't know yet fails and requeues — an eventually-consistent race with the
/// `OpenShellProvider` reconcile, not a terminal error.
async fn converge_providers(
    ctx: &Context,
    name: &str,
    desired: &[String],
    existing: &SandboxState,
    workspace: &str,
) -> Result<()> {
    let (attach, detach) = provider_delta(desired, &existing.providers);
    for provider in attach {
        info!(%name, %provider, "attaching provider");
        ctx.gateway
            .attach_provider(name, &provider, workspace)
            .await?;
    }
    for provider in detach {
        info!(%name, %provider, "detaching provider");
        ctx.gateway
            .detach_provider(name, &provider, workspace)
            .await?;
    }
    Ok(())
}

/// Set difference of desired vs. current provider names: `(to_attach, to_detach)`.
fn provider_delta(desired: &[String], current: &[String]) -> (Vec<String>, Vec<String>) {
    let current_set: BTreeSet<&str> = current.iter().map(String::as_str).collect();
    let desired_set: BTreeSet<&str> = desired.iter().map(String::as_str).collect();
    let attach = desired
        .iter()
        .filter(|provider| !current_set.contains(provider.as_str()))
        .cloned()
        .collect();
    let detach = current
        .iter()
        .filter(|provider| !desired_set.contains(provider.as_str()))
        .cloned()
        .collect();
    (attach, detach)
}

/// Whether a recorded hash has drifted from the desired one, warranting action.
///
/// Acts only when a hash was previously recorded and differs. A missing hash
/// (`None`) means the field was never applied under hash tracking, so the
/// existing sandbox is adopted as-is rather than needlessly recreated or
/// updated. Used for both the immutable-spec and mutable-policy hashes.
fn hash_drifted(applied: Option<&str>, desired: &str) -> bool {
    matches!(applied, Some(hash) if hash != desired)
}

/// Delete the gateway sandbox, wait for it to disappear, then create it afresh
/// with the already-resolved `policy`.
///
/// The poll deliberately blocks this reconcile worker for up to ~60s rather
/// than threading a delete/create state machine across reconciles — recreate is
/// a rare event and other resources reconcile on their own worker futures. The
/// gateway's `delete_sandbox` is idempotent, so a `RecreateTimeout` requeue that
/// re-enters this path and deletes again is harmless.
async fn recreate(
    ctx: &Context,
    name: &str,
    sandbox: &OpenShellSandbox,
    policy: Option<proto::SandboxPolicy>,
) -> Result<SandboxState> {
    let workspace = workspace_of(&sandbox.spec);
    ctx.gateway.delete_sandbox(name, workspace).await?;
    for _ in 0..RECREATE_POLL_ATTEMPTS {
        if ctx.gateway.get_sandbox(name, workspace).await?.is_none() {
            return create(ctx, name, sandbox, policy).await;
        }
        tokio::time::sleep(RECREATE_POLL_INTERVAL).await;
    }
    // Still terminating; bail out and let the requeue retry the create.
    Err(Error::RecreateTimeout {
        name: name.to_owned(),
    })
}

/// Create the gateway sandbox from the current spec and already-resolved policy.
async fn create(
    ctx: &Context,
    name: &str,
    sandbox: &OpenShellSandbox,
    policy: Option<proto::SandboxPolicy>,
) -> Result<SandboxState> {
    info!(%name, "creating sandbox on gateway");
    let create = build_sandbox_create(name, &sandbox.spec, policy);
    ctx.gateway.create_sandbox(create).await
}

/// Fingerprint of the fields that are immutable on a live gateway sandbox.
///
/// A change here forces a delete+recreate. It covers the immutable inputs:
/// `image`, `environment`, `gpu`, the volume mounts (wired into the pod at
/// create time), and the resolved policy's `landlock`/`process` (the
/// hard-immutable policy sections). The policy is passed in already resolved —
/// from `spec.policy` or a referenced `OpenShellPolicy` — so editing a shared
/// policy's immutable sections is retroactive and does trigger a recreate. It
/// excludes the policy's `networkPolicies` and `filesystem`, which are converged
/// in place; see [`mutable_policy_fingerprint`].
fn immutable_fingerprint(
    spec: &OpenShellSandboxSpec,
    policy: Option<&OpenShellPolicySpec>,
) -> String {
    let mut material = serde_json::Map::new();
    material.insert("image".to_owned(), json!(spec.image));
    material.insert("environment".to_owned(), json!(spec.environment));
    material.insert("gpu".to_owned(), json!(spec.gpu));
    material.insert(
        "landlock".to_owned(),
        json!(policy.and_then(|policy| policy.landlock.as_ref())),
    );
    material.insert(
        "process".to_owned(),
        json!(policy.and_then(|policy| policy.process.as_ref())),
    );
    material.insert(
        "volumes".to_owned(),
        json!(
            spec.volumes
                .iter()
                .map(|volume| json!({
                    "name": volume.name,
                    "mountPath": volume.mount_path,
                    "subPath": volume.sub_path,
                    "readOnly": volume.read_only,
                }))
                .collect::<Vec<_>>()
        ),
    );

    // Fields added after the initial schema are folded in only when set, so a
    // sandbox created before they existed keeps its original hash and is not
    // needlessly recreated on operator upgrade. Use the *normalized* values the
    // gateway actually receives (see `build_sandbox_create`) — a gpu count with
    // no GPU, or present-but-empty resources, is dropped there and so must not
    // move the hash either, or it would trigger a recreate that changes nothing.
    if let Some(count) = spec.gpu.then_some(spec.gpu_count).flatten() {
        material.insert("gpuCount".to_owned(), json!(count));
    }
    if let Some(log_level) = &spec.log_level {
        material.insert("logLevel".to_owned(), json!(log_level));
    }
    if let Some(resources) = resources_json(spec.resources.as_ref()) {
        material.insert("resources".to_owned(), json!(resources));
    }
    if let Some(runtime_class) = &spec.runtime_class_name {
        material.insert("runtimeClassName".to_owned(), json!(runtime_class));
    }
    if !spec.labels.is_empty() {
        material.insert("labels".to_owned(), json!(spec.labels));
    }
    if !spec.annotations.is_empty() {
        material.insert("annotations".to_owned(), json!(spec.annotations));
    }

    hash_material(&serde_json::Value::Object(material))
}

/// Fingerprint of the policy fields that are mutable on a live gateway sandbox.
///
/// A change here is converged in place via `UpdateConfig`, without recreating.
/// It covers exactly the mutable sections — `networkPolicies` (freely mutable)
/// and `filesystem` (additively mutable; a non-additive edit is rejected by the
/// gateway as [`Error::PolicyUpdateRejected`]). A `None` policy hashes to a
/// stable empty fingerprint.
fn mutable_policy_fingerprint(policy: Option<&OpenShellPolicySpec>) -> String {
    let material = json!({
        "networkPolicies": policy.map(|policy| &policy.network_policies),
        "filesystem": policy.and_then(|policy| policy.filesystem.as_ref()),
    });
    hash_material(&material)
}

/// Deterministic short hash of a JSON value. `serde_json::Map` is `BTreeMap`-
/// backed, so key order is stable and the rendering is deterministic.
fn hash_material(material: &serde_json::Value) -> String {
    let rendered = serde_json::to_string(material).unwrap_or_default();
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

/// A sandbox's policy resolved from its source, in both the forms the reconciler
/// needs: the source [`OpenShellPolicySpec`] for deterministic hashing, and the
/// validated proto for gateway calls. Both are `None` when no policy is set.
struct ResolvedPolicy {
    /// Source document, used to compute the spec/policy fingerprints.
    spec: Option<OpenShellPolicySpec>,
    /// Validated proto policy, handed to create / recreate / update.
    proto: Option<proto::SandboxPolicy>,
}

/// Resolve the sandbox's policy (if any) to its source spec and validated proto.
///
/// The document may be given inline (`spec.policy`) or by reference
/// (`spec.policyRef`, naming an `OpenShellPolicy` in the sandbox's namespace),
/// but not both. A referenced policy that is absent yields [`Error::PolicyNotFound`];
/// either source is run through the gateway's parser, so an invalid document
/// yields [`Error::PolicyInvalid`] rather than being silently dropped. The
/// caller decides whether such an error is fatal (create) or merely surfaced
/// (an already-running sandbox).
async fn resolve(
    ctx: &Context,
    namespace: &str,
    spec: &OpenShellSandboxSpec,
) -> Result<ResolvedPolicy> {
    let source = match select_policy_source(spec)? {
        PolicySource::None => None,
        PolicySource::Inline(inline) => Some(inline.clone()),
        PolicySource::Ref(policy_ref) => {
            let api: Api<OpenShellPolicy> = Api::namespaced(ctx.kube.clone(), namespace);
            let policy = api
                .get_opt(policy_ref)
                .await?
                .ok_or_else(|| Error::PolicyNotFound {
                    namespace: namespace.to_owned(),
                    name: policy_ref.to_owned(),
                })?;
            Some(policy.spec)
        }
    };
    let proto = source.as_ref().map(policy::to_proto).transpose()?;
    Ok(ResolvedPolicy {
        spec: source,
        proto,
    })
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
        gpu_count: spec.gpu_count,
        policy,
        driver_config: volumes::driver_config_json(name, &spec.volumes),
        log_level: spec.log_level.clone(),
        resources: resources_json(spec.resources.as_ref()),
        runtime_class_name: spec.runtime_class_name.clone(),
        labels: spec.labels.clone(),
        annotations: spec.annotations.clone(),
        workspace: workspace_of(spec).to_owned(),
    }
}

/// Shape the CR's `resources` into the gateway's `requests`/`limits` JSON Struct,
/// or `None` when no quantity is actually set (so it never forces an empty
/// template). The keys mirror what the gateway parses back into typed driver
/// resource requirements.
fn resources_json(resources: Option<&SandboxResources>) -> Option<serde_json::Value> {
    let resources = resources?;
    let has_quantity = |section: Option<&ResourceQuantities>| {
        section.is_some_and(|q| q.cpu.is_some() || q.memory.is_some())
    };
    if !has_quantity(resources.requests.as_ref()) && !has_quantity(resources.limits.as_ref()) {
        return None;
    }
    serde_json::to_value(resources).ok()
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
    if !ctx
        .gateway
        .delete_sandbox(&name, workspace_of(&sandbox.spec))
        .await?
    {
        info!(%name, "sandbox already absent on gateway");
    }

    if sandbox.spec.volume_retention == VolumeRetention::Delete {
        delete_provisioned_volumes(&ctx, &sandbox, &name).await?;
    }

    Ok(Action::await_change())
}

/// Delete the PVCs this sandbox provisioned (called only when `volumeRetention`
/// is `Delete`).
async fn delete_provisioned_volumes(
    ctx: &Context,
    sandbox: &OpenShellSandbox,
    name: &str,
) -> Result<()> {
    let namespace = sandbox.namespace().ok_or(Error::MissingNamespace)?;
    info!(%name, "deleting provisioned sandbox volumes");
    let api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.kube.clone(), &namespace);
    api.delete_collection(
        &DeleteParams::default(),
        &ListParams::default().labels(&volumes::selector(name)),
    )
    .await?;
    Ok(())
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

/// Requeue cadence after a successful reconcile. `Ready` is the only steady
/// phase, so it backs off to the drift-recheck cadence. Every other reachable
/// phase can still change async on the gateway — `Provisioning` is still
/// settling, and `Error` is not necessarily terminal (the gateway recomputes
/// phase from pod status each sync and can recover `Error → Ready`) — so poll
/// quickly to keep `.status.phase` fresh. (`Deleting` never reaches here; the
/// cleanup path returns `await_change`.)
fn success_requeue(phase: Phase) -> Duration {
    // Ready and Stopped are both resting states: nothing changes until someone
    // acts, so polling them fast would only add gateway calls.
    if matches!(phase, Phase::Ready | Phase::Stopped) {
        REQUEUE_INTERVAL
    } else {
        TRANSITIONAL_REQUEUE_INTERVAL
    }
}

/// Map the gateway's phase onto the CR's coarse phase.
fn map_phase(phase: SandboxPhase) -> Phase {
    match phase {
        SandboxPhase::Ready => Phase::Ready,
        SandboxPhase::Error => Phase::Error,
        SandboxPhase::Deleting => Phase::Deleting,
        SandboxPhase::Stopped => Phase::Stopped,
        // Provisioning, Stopping, Starting, Unspecified, and Unknown are all
        // settling, so the catch-all reads them as Provisioning. A future
        // variant that rests instead of settling needs its own arm, or it gets
        // reported as Provisioning for as long as it lasts.
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
        Phase, PolicySource, REQUEUE_INTERVAL, TRANSITIONAL_REQUEUE_INTERVAL, build_sandbox_create,
        hash_drifted, hash_material, immutable_fingerprint, map_phase, mutable_policy_fingerprint,
        provider_delta, references_policy, resources_json, select_policy_source, success_requeue,
        workspace_of,
    };
    use crate::crd::{
        FilesystemPolicy, LandlockPolicy, OpenShellPolicySpec, OpenShellSandbox,
        OpenShellSandboxSpec, PreservedValue, ProcessPolicy, ResourceQuantities, SandboxResources,
        SandboxVolume,
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
    fn workspace_of_defaults_to_empty_when_unset() {
        // Unset → empty string, which the gateway resolves to its `default`
        // workspace — preserving the pre-workspace behaviour exactly.
        assert_eq!(workspace_of(&OpenShellSandboxSpec::default()), "");
        let scoped = OpenShellSandboxSpec {
            workspace: Some("team-a".to_owned()),
            ..OpenShellSandboxSpec::default()
        };
        assert_eq!(workspace_of(&scoped), "team-a");
    }

    #[test]
    fn build_sandbox_create_carries_workspace() {
        let scoped = OpenShellSandboxSpec {
            workspace: Some("team-a".to_owned()),
            ..OpenShellSandboxSpec::default()
        };
        assert_eq!(
            build_sandbox_create("named", &scoped, None).workspace,
            "team-a"
        );
        // Unset stays empty (the gateway default), not the literal "default".
        assert_eq!(
            build_sandbox_create("named", &sample_spec(), None).workspace,
            ""
        );
    }

    #[test]
    fn workspace_absent_from_fingerprint() {
        // Workspace is immutable and identity-defining, not a recreate trigger:
        // setting it must not perturb the immutable fingerprint, so existing
        // sandboxes keep their recorded hash across the upgrade that adds it.
        let base = immutable_fingerprint(&sample_spec(), None);
        let scoped = OpenShellSandboxSpec {
            workspace: Some("team-a".to_owned()),
            ..sample_spec()
        };
        assert_eq!(immutable_fingerprint(&scoped, None), base);
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

    fn spec_with_primitives() -> OpenShellSandboxSpec {
        OpenShellSandboxSpec {
            gpu: true,
            gpu_count: Some(2),
            log_level: Some("debug".to_owned()),
            runtime_class_name: Some("gvisor".to_owned()),
            labels: std::collections::BTreeMap::from([("team".to_owned(), "core".to_owned())]),
            annotations: std::collections::BTreeMap::from([("note".to_owned(), "x".to_owned())]),
            resources: Some(SandboxResources {
                requests: Some(ResourceQuantities {
                    cpu: Some("500m".to_owned()),
                    memory: Some("256Mi".to_owned()),
                }),
                limits: Some(ResourceQuantities {
                    cpu: Some("2".to_owned()),
                    memory: None,
                }),
            }),
            ..OpenShellSandboxSpec::default()
        }
    }

    #[test]
    fn build_sandbox_create_maps_new_primitives() {
        let create = build_sandbox_create("named", &spec_with_primitives(), None);
        assert_eq!(create.gpu_count, Some(2));
        assert_eq!(create.log_level.as_deref(), Some("debug"));
        assert_eq!(create.runtime_class_name.as_deref(), Some("gvisor"));
        assert_eq!(create.labels.get("team").map(String::as_str), Some("core"));
        assert_eq!(
            create.annotations.get("note").map(String::as_str),
            Some("x")
        );
        let resources = create.resources.expect("resources present");
        assert_eq!(resources["requests"]["cpu"], "500m");
        assert_eq!(resources["requests"]["memory"], "256Mi");
        assert_eq!(resources["limits"]["cpu"], "2");
    }

    #[test]
    fn resources_json_none_when_no_quantities() {
        // Present-but-empty resources must not force an (empty) template.
        assert!(resources_json(None).is_none());
        let empty = SandboxResources {
            requests: Some(ResourceQuantities::default()),
            limits: None,
        };
        assert!(resources_json(Some(&empty)).is_none());
    }

    #[test]
    fn new_primitives_drift_forces_recreate() {
        // Each new immutable field, once set, changes the fingerprint so the
        // reconciler recreates the sandbox on a change.
        let base = immutable_fingerprint(&OpenShellSandboxSpec::default(), None);
        let mutate = |f: fn(&mut OpenShellSandboxSpec)| {
            let mut spec = OpenShellSandboxSpec::default();
            f(&mut spec);
            immutable_fingerprint(&spec, None)
        };
        // gpuCount only counts once a GPU is requested (it is dropped otherwise),
        // so compare against a GPU-enabled baseline.
        let gpu_on = immutable_fingerprint(
            &OpenShellSandboxSpec {
                gpu: true,
                ..OpenShellSandboxSpec::default()
            },
            None,
        );
        let gpu_counted = immutable_fingerprint(
            &OpenShellSandboxSpec {
                gpu: true,
                gpu_count: Some(1),
                ..OpenShellSandboxSpec::default()
            },
            None,
        );
        assert_ne!(gpu_on, gpu_counted);
        assert_ne!(base, mutate(|s| s.log_level = Some("debug".to_owned())));
        assert_ne!(
            base,
            mutate(|s| s.runtime_class_name = Some("kata".to_owned()))
        );
        assert_ne!(
            base,
            mutate(
                |s| s.labels = std::collections::BTreeMap::from([("a".to_owned(), "b".to_owned())])
            )
        );
        assert_ne!(
            base,
            mutate(|s| s.annotations =
                std::collections::BTreeMap::from([("a".to_owned(), "b".to_owned())]))
        );
        assert_ne!(
            base,
            mutate(|s| s.resources = Some(SandboxResources {
                requests: Some(ResourceQuantities {
                    cpu: Some("1".to_owned()),
                    memory: None,
                }),
                limits: None,
            }))
        );
    }

    #[test]
    fn fingerprint_unchanged_for_pre_primitives_specs() {
        // Upgrade safety: a spec using only the original fields must hash exactly
        // as the operator did before these primitives existed, so upgrading does
        // not needlessly recreate running sandboxes. This reconstructs the legacy
        // material and asserts the current fingerprint matches it.
        let spec = OpenShellSandboxSpec {
            image: Some("img".to_owned()),
            gpu: true,
            ..OpenShellSandboxSpec::default()
        };
        let legacy = serde_json::json!({
            "image": spec.image,
            "environment": spec.environment,
            "gpu": spec.gpu,
            "landlock": Option::<()>::None,
            "process": Option::<()>::None,
            "volumes": Vec::<serde_json::Value>::new(),
        });
        assert_eq!(immutable_fingerprint(&spec, None), hash_material(&legacy));
    }

    #[test]
    fn normalized_primitives_do_not_drift_fingerprint() {
        // Values the gateway drops in `build_sandbox_create` must not move the
        // hash, or they'd force a recreate that sends nothing new: a gpu count
        // with no GPU, and present-but-empty resources.
        let base = immutable_fingerprint(&OpenShellSandboxSpec::default(), None);

        let gpu_count_without_gpu = OpenShellSandboxSpec {
            gpu: false,
            gpu_count: Some(4),
            ..OpenShellSandboxSpec::default()
        };
        assert_eq!(base, immutable_fingerprint(&gpu_count_without_gpu, None));

        let empty_resources = OpenShellSandboxSpec {
            resources: Some(SandboxResources {
                requests: Some(ResourceQuantities::default()),
                limits: None,
            }),
            ..OpenShellSandboxSpec::default()
        };
        assert_eq!(base, immutable_fingerprint(&empty_resources, None));
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
        assert_eq!(map_phase(SandboxPhase::Stopped), Phase::Stopped);
        // Unspecified/unknown settle as provisioning.
        assert_eq!(map_phase(SandboxPhase::Unspecified), Phase::Provisioning);
        // Stopping and Starting are on the way somewhere, so they settle as
        // provisioning. Stopped is the resting state and maps on its own.
        assert_eq!(map_phase(SandboxPhase::Stopping), Phase::Provisioning);
        assert_eq!(map_phase(SandboxPhase::Starting), Phase::Provisioning);
    }

    #[test]
    fn success_requeue_polls_faster_while_settling() {
        // Ready is steady → drift cadence.
        assert_eq!(success_requeue(Phase::Ready), REQUEUE_INTERVAL);
        // Stopped rests until someone starts it, so it takes the same cadence.
        // A short poll here would call the gateway every 10s for good.
        assert_eq!(success_requeue(Phase::Stopped), REQUEUE_INTERVAL);
        // Still-changeable phases → short poll so `.status.phase` catches up
        // quickly. Error is included: the gateway can recover Error → Ready.
        assert_eq!(
            success_requeue(Phase::Provisioning),
            TRANSITIONAL_REQUEUE_INTERVAL
        );
        assert_eq!(success_requeue(Phase::Error), TRANSITIONAL_REQUEUE_INTERVAL);
        assert!(TRANSITIONAL_REQUEUE_INTERVAL < REQUEUE_INTERVAL);
    }

    fn owned(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn provider_delta_attaches_missing_and_detaches_extra() {
        let (attach, detach) = provider_delta(&owned(&["a", "c"]), &owned(&["a", "b"]));
        assert_eq!(attach, owned(&["c"]));
        assert_eq!(detach, owned(&["b"]));
    }

    #[test]
    fn provider_delta_is_empty_when_converged() {
        let (attach, detach) = provider_delta(&owned(&["a", "b"]), &owned(&["b", "a"]));
        assert!(attach.is_empty());
        assert!(detach.is_empty());
    }

    #[test]
    fn provider_delta_handles_empty_sides() {
        let (attach, detach) = provider_delta(&owned(&["a"]), &[]);
        assert_eq!(attach, owned(&["a"]));
        assert!(detach.is_empty());

        let (attach, detach) = provider_delta(&[], &owned(&["a"]));
        assert!(attach.is_empty());
        assert_eq!(detach, owned(&["a"]));
    }

    #[test]
    fn hash_drift_adopts_when_no_prior_hash() {
        // No recorded hash: adopt the existing sandbox, never act.
        assert!(!hash_drifted(None, "abc"));
    }

    #[test]
    fn hash_drift_only_on_changed_hash() {
        assert!(!hash_drifted(Some("abc"), "abc"));
        assert!(hash_drifted(Some("abc"), "def"));
    }

    fn landlock() -> OpenShellPolicySpec {
        OpenShellPolicySpec {
            landlock: Some(LandlockPolicy {
                compatibility: "enforce".to_owned(),
            }),
            ..OpenShellPolicySpec::default()
        }
    }

    fn filesystem() -> OpenShellPolicySpec {
        OpenShellPolicySpec {
            filesystem: Some(FilesystemPolicy {
                include_workdir: true,
                read_only: vec!["/etc".to_owned()],
                read_write: Vec::new(),
            }),
            ..OpenShellPolicySpec::default()
        }
    }

    fn network() -> OpenShellPolicySpec {
        OpenShellPolicySpec {
            network_policies: std::iter::once((
                "claude_code".to_owned(),
                PreservedValue(serde_json::json!({ "endpoints": [] })),
            ))
            .collect(),
            ..OpenShellPolicySpec::default()
        }
    }

    #[test]
    fn fingerprint_is_stable_for_equal_specs() {
        assert_eq!(
            immutable_fingerprint(&sample_spec(), None),
            immutable_fingerprint(&sample_spec(), None)
        );
    }

    #[test]
    fn fingerprint_changes_on_immutable_spec_fields() {
        let base = immutable_fingerprint(&sample_spec(), None);

        let mut image = sample_spec();
        image.image = Some("ghcr.io/example/sandbox:v2".to_owned());
        assert_ne!(immutable_fingerprint(&image, None), base);

        let mut gpu = sample_spec();
        gpu.gpu = false;
        assert_ne!(immutable_fingerprint(&gpu, None), base);

        let mut env = sample_spec();
        env.environment.insert("LOG".to_owned(), "debug".to_owned());
        assert_ne!(immutable_fingerprint(&env, None), base);

        let mut volume = sample_spec();
        volume.volumes.push(SandboxVolume {
            name: "data".to_owned(),
            mount_path: "/data".to_owned(),
            sub_path: None,
            read_only: false,
            claim: PersistentVolumeClaimSpec::default(),
        });
        assert_ne!(immutable_fingerprint(&volume, None), base);
    }

    #[test]
    fn fingerprint_changes_on_immutable_policy_sections() {
        let base = immutable_fingerprint(&sample_spec(), None);

        // The resolved policy's landlock/process fold into the recreate hash,
        // whether they came from an inline document or a referenced one.
        assert_ne!(
            immutable_fingerprint(&sample_spec(), Some(&landlock())),
            base
        );

        let process = OpenShellPolicySpec {
            process: Some(ProcessPolicy {
                run_as_user: "nobody".to_owned(),
                run_as_group: String::new(),
            }),
            ..OpenShellPolicySpec::default()
        };
        assert_ne!(immutable_fingerprint(&sample_spec(), Some(&process)), base);
    }

    #[test]
    fn fingerprint_ignores_mutable_policy_sections() {
        // Mutable sections are converged in place, so they must not change the
        // recreate fingerprint.
        let base = immutable_fingerprint(&sample_spec(), None);
        assert_eq!(
            immutable_fingerprint(&sample_spec(), Some(&filesystem())),
            base
        );
        assert_eq!(
            immutable_fingerprint(&sample_spec(), Some(&network())),
            base
        );
    }

    #[test]
    fn mutable_fingerprint_tracks_mutable_sections() {
        let base = mutable_policy_fingerprint(None);
        assert_eq!(base, mutable_policy_fingerprint(None));

        // Mutable sections change the mutable hash (→ in-place UpdateConfig).
        assert_ne!(mutable_policy_fingerprint(Some(&filesystem())), base);
        assert_ne!(mutable_policy_fingerprint(Some(&network())), base);
    }

    #[test]
    fn mutable_fingerprint_ignores_immutable_sections() {
        // landlock/process drive recreation, not in-place update, so adding them
        // to a policy must not perturb its mutable hash.
        let network_only = network();
        let mut with_landlock = network();
        with_landlock.landlock = landlock().landlock;
        assert_eq!(
            mutable_policy_fingerprint(Some(&network_only)),
            mutable_policy_fingerprint(Some(&with_landlock)),
        );
    }

    fn sandbox_with(namespace: &str, policy_ref: Option<&str>) -> OpenShellSandbox {
        let mut sandbox = OpenShellSandbox::new(
            "box",
            OpenShellSandboxSpec {
                policy_ref: policy_ref.map(str::to_owned),
                ..OpenShellSandboxSpec::default()
            },
        );
        sandbox.metadata.namespace = Some(namespace.to_owned());
        sandbox
    }

    #[test]
    fn references_policy_matches_same_namespace_and_ref() {
        let sandbox = sandbox_with("team-a", Some("restricted"));
        assert!(references_policy(&sandbox, Some("team-a"), "restricted"));
    }

    #[test]
    fn references_policy_ignores_other_namespace_name_or_unset_ref() {
        let sandbox = sandbox_with("team-a", Some("restricted"));
        assert!(!references_policy(&sandbox, Some("team-b"), "restricted"));
        assert!(!references_policy(&sandbox, Some("team-a"), "other"));

        let inline = sandbox_with("team-a", None);
        assert!(!references_policy(&inline, Some("team-a"), "restricted"));
    }
}
