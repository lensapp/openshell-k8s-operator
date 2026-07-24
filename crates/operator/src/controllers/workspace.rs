// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconciliation loop for [`OpenShellWorkspace`].
//!
//! An `OpenShellWorkspace` is cluster-scoped and maps 1:1 to a gateway
//! workspace whose name is the resource's `metadata.name`. The loop creates the
//! workspace on the gateway (adopting one that already exists), converges its
//! membership when `spec.members` is set, and mirrors the gateway phase into
//! `.status`. A finalizer guards deletion: because the gateway marks a workspace
//! terminating *before* it checks for blockers — with no undelete — deleting a
//! workspace that still holds sandboxes or providers would permanently wedge it,
//! so the finalizer refuses while any `OpenShellSandbox`/`OpenShellProvider`
//! still references the workspace, and it never deletes the built-in `default`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    Api, Resource, ResourceExt,
    api::{ListParams, Patch, PatchParams},
    runtime::{
        Controller,
        controller::Action,
        events::EventType,
        finalizer::{Event as Finalizer, finalizer},
        watcher,
    },
};
use tracing::{info, warn};

use super::{
    Context, ERROR_REQUEUE_INTERVAL, REQUEUE_INTERVAL, TRANSITIONAL_REQUEUE_INTERVAL, record_event,
    record_failure,
};
use crate::conditions;
use crate::crd::{
    OpenShellProvider, OpenShellSandbox, OpenShellWorkspace, OpenShellWorkspaceStatus,
    WorkspaceMember, WorkspacePhase, WorkspaceRole,
};
use crate::error::{Error, Result};
use crate::gateway::{
    WorkspaceCreate, WorkspaceMemberView, WorkspacePhase as GatewayWorkspacePhase,
    WorkspaceRole as GatewayWorkspaceRole,
};

/// Finalizer key guaranteeing gateway-side deletion before the CR is removed.
pub const FINALIZER: &str = "openshell.lenshq.io/workspace-cleanup";

/// The gateway's built-in workspace. It is created implicitly, cannot be
/// deleted, and is the target of an empty/omitted `spec.workspace` elsewhere.
const DEFAULT_WORKSPACE: &str = "default";

/// Run the workspace controller until the process is stopped.
pub async fn run(ctx: Arc<Context>) {
    let workspaces: Api<OpenShellWorkspace> = Api::all(ctx.kube.clone());

    Controller::new(workspaces, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((obj, _)) => info!(workspace = %obj.name, "reconciled"),
                Err(err) => warn!(error = %err, "workspace reconcile loop error"),
            }
        })
        .await;
}

async fn reconcile(workspace: Arc<OpenShellWorkspace>, ctx: Arc<Context>) -> Result<Action> {
    // Cluster-scoped: the object has no namespace, so the API is cluster-wide.
    let api: Api<OpenShellWorkspace> = Api::all(ctx.kube.clone());

    finalizer(&api, FINALIZER, workspace, |event| async {
        match event {
            Finalizer::Apply(obj) => apply(obj, ctx.clone()).await,
            Finalizer::Cleanup(obj) => cleanup(obj, ctx.clone()).await,
        }
    })
    .await
    .map_err(|err| Error::Finalizer(Box::new(err)))
}

/// Create-or-adopt the gateway workspace, converge its membership, and mirror
/// the gateway phase into `.status`. `Ready` is written on both success and
/// failure so the outcome surfaces on the resource, not only in the log.
async fn apply(workspace: Arc<OpenShellWorkspace>, ctx: Arc<Context>) -> Result<Action> {
    let name = workspace.name_any();
    info!(%name, "reconciling OpenShellWorkspace");

    let generation = workspace.meta().generation;
    let now = Time(chrono::Utc::now());
    let prior = workspace.status.clone().unwrap_or_default();
    let mut current = prior.conditions.clone();

    match converge(&ctx, &workspace, &name).await {
        Ok(phase) => {
            conditions::set(
                &mut current,
                conditions::condition(
                    conditions::READY,
                    true,
                    "Reconciled",
                    "workspace reconciled with the gateway",
                    generation,
                    now,
                ),
            );
            let status = OpenShellWorkspaceStatus {
                conditions: current,
                phase: map_phase(phase),
                observed_generation: generation,
            };
            patch_status(&ctx, &name, &status).await?;
            Ok(Action::requeue(success_requeue(phase)))
        }
        Err(err) => {
            record_failure(&ctx, workspace.as_ref(), "Reconcile", &err).await;
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
            // Keep any prior gateway phase for visibility.
            let status = OpenShellWorkspaceStatus {
                conditions: current,
                phase: prior.phase,
                observed_generation: generation,
            };
            if let Err(patch_err) = patch_status(&ctx, &name, &status).await {
                warn!(error = %patch_err, "failed to record failure status");
            }
            Err(err)
        }
    }
}

/// Ensure the gateway workspace exists (adopting an existing one), then converge
/// membership when it is managed. Returns the gateway's current phase.
async fn converge(
    ctx: &Context,
    workspace: &OpenShellWorkspace,
    name: &str,
) -> Result<GatewayWorkspacePhase> {
    let state = if let Some(state) = ctx.gateway.get_workspace(name).await? {
        state
    } else {
        info!(%name, "creating workspace on gateway");
        ctx.gateway
            .create_workspace(WorkspaceCreate {
                name: name.to_owned(),
                labels: workspace.spec.labels.clone(),
            })
            .await?
    };

    if let Some(members) = &workspace.spec.members {
        converge_members(ctx, name, members).await?;
    }

    Ok(state.phase)
}

/// Converge the workspace's members toward the declared set: remove those no
/// longer wanted (or whose role changed), then add those missing (or with a new
/// role). Ordered remove-before-add because the gateway's add is create-only and
/// a role change must drop the old grant first.
async fn converge_members(
    ctx: &Context,
    workspace: &str,
    desired: &[WorkspaceMember],
) -> Result<()> {
    let current = ctx.gateway.list_workspace_members(workspace).await?;
    let (removes, adds) = member_delta(desired, &current);

    for subject in removes {
        info!(%workspace, %subject, "removing workspace member");
        ctx.gateway
            .remove_workspace_member(workspace, &subject)
            .await?;
    }
    for (subject, role) in adds {
        info!(%workspace, %subject, "adding workspace member");
        ctx.gateway
            .add_workspace_member(workspace, &subject, role)
            .await?;
    }
    Ok(())
}

/// Diff the desired members against the gateway's current set, keyed on subject.
/// Returns `(removes, adds)` in execution order — a role change appears in
/// *both* (remove the old grant, then add the new one, because the gateway's add
/// is create-only). Pure, so it is unit-tested in isolation.
fn member_delta(
    desired: &[WorkspaceMember],
    current: &[WorkspaceMemberView],
) -> (Vec<String>, Vec<(String, GatewayWorkspaceRole)>) {
    let desired: BTreeMap<&str, GatewayWorkspaceRole> = desired
        .iter()
        .map(|member| (member.subject.as_str(), to_gateway_role(member.role)))
        .collect();
    let current: BTreeMap<&str, GatewayWorkspaceRole> = current
        .iter()
        .map(|member| (member.subject.as_str(), member.role))
        .collect();

    // Remove anything the gateway has that the desired set does not match
    // exactly (absent, or a differing role).
    let removes = current
        .iter()
        .filter(|(subject, role)| desired.get(*subject) != Some(role))
        .map(|(subject, _)| (*subject).to_owned())
        .collect();

    // Add anything desired the gateway does not already have at the right role.
    let adds = desired
        .iter()
        .filter(|(subject, role)| current.get(*subject) != Some(role))
        .map(|(subject, role)| ((*subject).to_owned(), *role))
        .collect();

    (removes, adds)
}

/// Delete the gateway workspace before the finalizer releases the CR — but only
/// once it is safe. The gateway marks a workspace terminating before checking
/// for blockers and offers no undelete, so a delete that the gateway would
/// reject leaves the workspace permanently unusable. Guard against that: never
/// delete the built-in `default`, and refuse while any sandbox or provider still
/// references this workspace (surfacing [`Error::WorkspaceNotEmpty`] so the CR
/// lingers until it is emptied). Out-of-band references the operator cannot see
/// still surface as a gateway `FailedPrecondition`, which requeues.
async fn cleanup(workspace: Arc<OpenShellWorkspace>, ctx: Arc<Context>) -> Result<Action> {
    let name = workspace.name_any();

    if name == DEFAULT_WORKSPACE {
        info!("default workspace is not deletable; releasing finalizer");
        return Ok(Action::await_change());
    }

    let referencing = count_referencing(&ctx, &name).await?;
    if referencing > 0 {
        return Err(refuse_non_empty(&ctx, workspace.as_ref(), &name, referencing).await);
    }

    delete_gateway_workspace(&ctx, &name).await?;
    Ok(Action::await_change())
}

/// Delete the workspace on the gateway, treating an already-absent one as done.
async fn delete_gateway_workspace(ctx: &Context, name: &str) -> Result<()> {
    info!(%name, "deleting workspace on gateway");
    if !ctx.gateway.delete_workspace(name).await? {
        info!(%name, "workspace already absent on gateway");
    }
    Ok(())
}

/// Refuse to delete a workspace that still has referencing resources, returning
/// the [`Error::WorkspaceNotEmpty`] that keeps the finalizer (and thus the CR) in
/// place until it is emptied. The refusal is surfaced two ways: a `Warning`
/// event, and — since a deleting CR lingers indefinitely — the `Ready` condition,
/// so `kubectl get` and GitOps tooling see *why* rather than a stale `Ready`.
async fn refuse_non_empty(
    ctx: &Context,
    workspace: &OpenShellWorkspace,
    name: &str,
    count: usize,
) -> Error {
    let err = Error::WorkspaceNotEmpty {
        name: name.to_owned(),
        count,
    };
    record_event(
        ctx,
        workspace,
        EventType::Warning,
        err.reason(),
        "Delete",
        err.to_string(),
    )
    .await;

    let prior = workspace.status.clone().unwrap_or_default();
    let mut conditions = prior.conditions.clone();
    conditions::set(
        &mut conditions,
        conditions::condition(
            conditions::READY,
            false,
            err.reason(),
            err.to_string(),
            workspace.meta().generation,
            Time(chrono::Utc::now()),
        ),
    );
    let status = OpenShellWorkspaceStatus {
        conditions,
        ..prior
    };
    if let Err(patch_err) = patch_status(ctx, name, &status).await {
        warn!(error = %patch_err, "failed to record blocked-deletion status");
    }
    err
}

/// Count the sandboxes and providers, cluster-wide, that reference `workspace`.
/// Used by the finalizer pre-flight; a direct list (not an informer) is fine on
/// the rare delete path.
async fn count_referencing(ctx: &Context, workspace: &str) -> Result<usize> {
    let sandboxes: Api<OpenShellSandbox> = Api::all(ctx.kube.clone());
    let providers: Api<OpenShellProvider> = Api::all(ctx.kube.clone());

    let sandbox_refs = sandboxes
        .list(&ListParams::default())
        .await?
        .into_iter()
        .filter(|sandbox| normalize_workspace(sandbox.spec.workspace.as_deref()) == workspace)
        .count();
    let provider_refs = providers
        .list(&ListParams::default())
        .await?
        .into_iter()
        .filter(|provider| normalize_workspace(provider.spec.workspace.as_deref()) == workspace)
        .count();

    Ok(sandbox_refs + provider_refs)
}

/// Resolve a `spec.workspace` value to the concrete workspace name it targets:
/// an unset or empty value is the gateway's `default`. Matched against the value
/// the sandbox/provider controllers actually send (their `workspace_of`), so it
/// deliberately does not trim — the comparison is on exactly what was sent.
fn normalize_workspace(workspace: Option<&str>) -> &str {
    match workspace {
        Some(name) if !name.is_empty() => name,
        _ => DEFAULT_WORKSPACE,
    }
}

/// Map the gateway phase onto the CR's coarse phase; an unknown phase is left
/// unset rather than guessed.
fn map_phase(phase: GatewayWorkspacePhase) -> Option<WorkspacePhase> {
    match phase {
        GatewayWorkspacePhase::Active => Some(WorkspacePhase::Active),
        GatewayWorkspacePhase::Terminating => Some(WorkspacePhase::Terminating),
        GatewayWorkspacePhase::Unknown => None,
    }
}

/// Map a CR member role onto the gateway role.
fn to_gateway_role(role: WorkspaceRole) -> GatewayWorkspaceRole {
    match role {
        WorkspaceRole::User => GatewayWorkspaceRole::User,
        WorkspaceRole::Admin => GatewayWorkspaceRole::Admin,
    }
}

/// Requeue cadence after a successful reconcile. `Active` is steady, so it backs
/// off to the drift-recheck cadence; a `Terminating` (or unknown) workspace is
/// still changing, so poll quickly to keep `.status.phase` fresh.
fn success_requeue(phase: GatewayWorkspacePhase) -> Duration {
    if phase == GatewayWorkspacePhase::Active {
        REQUEUE_INTERVAL
    } else {
        TRANSITIONAL_REQUEUE_INTERVAL
    }
}

async fn patch_status(ctx: &Context, name: &str, status: &OpenShellWorkspaceStatus) -> Result<()> {
    let api: Api<OpenShellWorkspace> = Api::all(ctx.kube.clone());
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

fn error_policy(_workspace: Arc<OpenShellWorkspace>, err: &Error, _ctx: Arc<Context>) -> Action {
    warn!(error = %err, "workspace reconcile failed; requeueing");
    Action::requeue(ERROR_REQUEUE_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::{map_phase, member_delta, normalize_workspace, success_requeue, to_gateway_role};
    use crate::controllers::{REQUEUE_INTERVAL, TRANSITIONAL_REQUEUE_INTERVAL};
    use crate::crd::{WorkspaceMember, WorkspacePhase, WorkspaceRole};
    use crate::gateway::{
        WorkspaceMemberView, WorkspacePhase as GatewayWorkspacePhase,
        WorkspaceRole as GatewayWorkspaceRole,
    };

    fn desired(subject: &str, role: WorkspaceRole) -> WorkspaceMember {
        WorkspaceMember {
            subject: subject.to_owned(),
            role,
        }
    }

    fn have(subject: &str, role: GatewayWorkspaceRole) -> WorkspaceMemberView {
        WorkspaceMemberView {
            subject: subject.to_owned(),
            role,
        }
    }

    #[test]
    fn member_delta_adds_missing_and_removes_extra() {
        let (removes, adds) = member_delta(
            &[desired("alice", WorkspaceRole::Admin)],
            &[have("bob", GatewayWorkspaceRole::User)],
        );
        assert_eq!(
            adds,
            vec![("alice".to_owned(), GatewayWorkspaceRole::Admin)]
        );
        assert_eq!(removes, vec!["bob".to_owned()]);
    }

    #[test]
    fn member_delta_is_empty_when_converged() {
        let (removes, adds) = member_delta(
            &[
                desired("alice", WorkspaceRole::Admin),
                desired("bob", WorkspaceRole::User),
            ],
            &[
                have("bob", GatewayWorkspaceRole::User),
                have("alice", GatewayWorkspaceRole::Admin),
            ],
        );
        assert!(adds.is_empty());
        assert!(removes.is_empty());
    }

    #[test]
    fn member_delta_role_change_removes_then_adds() {
        // A changed role must appear in both lists: the gateway's add is
        // create-only, so the old grant is dropped before the new one is added.
        let (removes, adds) = member_delta(
            &[desired("alice", WorkspaceRole::Admin)],
            &[have("alice", GatewayWorkspaceRole::User)],
        );
        assert_eq!(
            adds,
            vec![("alice".to_owned(), GatewayWorkspaceRole::Admin)]
        );
        assert_eq!(removes, vec!["alice".to_owned()]);
    }

    #[test]
    fn member_delta_empty_desired_removes_all() {
        // An empty (but present) member set is authoritative: strip everyone.
        let (removes, adds) = member_delta(&[], &[have("alice", GatewayWorkspaceRole::Admin)]);
        assert!(adds.is_empty());
        assert_eq!(removes, vec!["alice".to_owned()]);
    }

    #[test]
    fn normalize_workspace_defaults_when_unset_or_empty() {
        assert_eq!(normalize_workspace(None), "default");
        assert_eq!(normalize_workspace(Some("")), "default");
        assert_eq!(normalize_workspace(Some("team-a")), "team-a");
    }

    #[test]
    fn map_phase_leaves_unknown_unset() {
        assert_eq!(
            map_phase(GatewayWorkspacePhase::Active),
            Some(WorkspacePhase::Active)
        );
        assert_eq!(
            map_phase(GatewayWorkspacePhase::Terminating),
            Some(WorkspacePhase::Terminating)
        );
        assert_eq!(map_phase(GatewayWorkspacePhase::Unknown), None);
    }

    #[test]
    fn role_maps_to_gateway() {
        assert_eq!(
            to_gateway_role(WorkspaceRole::User),
            GatewayWorkspaceRole::User
        );
        assert_eq!(
            to_gateway_role(WorkspaceRole::Admin),
            GatewayWorkspaceRole::Admin
        );
    }

    #[test]
    fn success_requeue_polls_faster_until_active() {
        assert_eq!(
            success_requeue(GatewayWorkspacePhase::Active),
            REQUEUE_INTERVAL
        );
        assert_eq!(
            success_requeue(GatewayWorkspacePhase::Terminating),
            TRANSITIONAL_REQUEUE_INTERVAL
        );
    }
}
