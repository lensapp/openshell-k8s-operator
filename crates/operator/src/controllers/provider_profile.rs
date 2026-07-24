// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconciliation loop for [`OpenShellProviderProfile`].
//!
//! An `OpenShellProviderProfile` is cluster-scoped and maps 1:1 to a
//! platform-scoped gateway provider-type profile whose id is the resource's
//! `metadata.name`. The loop validates the profile document with the gateway's
//! own `openshell-providers` parser, imports it (or updates it in place, with
//! optimistic concurrency on the gateway's stored resource version), and mirrors
//! that version into `.status`. It owns gateway state, so a finalizer guards
//! deletion: it refuses while any `OpenShellProvider` still selects this type,
//! since deleting the profile would break those providers' credential handling.

use std::sync::Arc;

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

use super::{Context, ERROR_REQUEUE_INTERVAL, REQUEUE_INTERVAL, record_event, record_failure};
use crate::conditions;
use crate::crd::{OpenShellProvider, OpenShellProviderProfile, OpenShellProviderProfileStatus};
use crate::error::{Error, Result};
use crate::profile;

/// Finalizer key guaranteeing gateway-side deletion before the CR is removed.
pub const FINALIZER: &str = "openshell.lenshq.io/provider-profile-cleanup";

/// Run the provider-profile controller until the process is stopped.
pub async fn run(ctx: Arc<Context>) {
    let profiles: Api<OpenShellProviderProfile> = Api::all(ctx.kube.clone());

    Controller::new(profiles, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((obj, _)) => info!(profile = %obj.name, "reconciled"),
                Err(err) => warn!(error = %err, "provider profile reconcile loop error"),
            }
        })
        .await;
}

async fn reconcile(profile: Arc<OpenShellProviderProfile>, ctx: Arc<Context>) -> Result<Action> {
    // Cluster-scoped: the object has no namespace, so the API is cluster-wide.
    let api: Api<OpenShellProviderProfile> = Api::all(ctx.kube.clone());

    finalizer(&api, FINALIZER, profile, |event| async {
        match event {
            Finalizer::Apply(obj) => apply(obj, ctx.clone()).await,
            Finalizer::Cleanup(obj) => cleanup(obj, ctx.clone()).await,
        }
    })
    .await
    .map_err(|err| Error::Finalizer(Box::new(err)))
}

/// Validate the profile document, import/update it on the gateway, and record
/// the outcome. `Ready` is written on both success and failure so the result
/// surfaces on the resource, not only in the log.
async fn apply(profile: Arc<OpenShellProviderProfile>, ctx: Arc<Context>) -> Result<Action> {
    let name = profile.name_any();
    info!(%name, "reconciling OpenShellProviderProfile");

    let generation = profile.meta().generation;
    let now = Time(chrono::Utc::now());
    let prior = profile.status.clone().unwrap_or_default();
    let mut current = prior.conditions.clone();

    // Convert the spec to a proto profile (validating it via the gateway parser)
    // and upsert it; the upsert returns the gateway's stored resource version.
    let synced = async {
        let proto = profile::to_proto(&name, &profile.spec)?;
        ctx.gateway.upsert_provider_profile(proto).await
    }
    .await;

    match synced {
        Ok(resource_version) => {
            conditions::set(
                &mut current,
                conditions::condition(
                    conditions::READY,
                    true,
                    "Reconciled",
                    "profile synced to the gateway",
                    generation,
                    now,
                ),
            );
            let status = OpenShellProviderProfileStatus {
                conditions: current,
                observed_generation: generation,
                resource_version: Some(resource_version),
            };
            patch_status(&ctx, &name, &status).await?;
            Ok(Action::requeue(REQUEUE_INTERVAL))
        }
        Err(err) => {
            record_failure(&ctx, profile.as_ref(), "Import", &err).await;
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
            // Keep any prior stored version for visibility.
            let status = OpenShellProviderProfileStatus {
                conditions: current,
                observed_generation: generation,
                resource_version: prior.resource_version,
            };
            if let Err(patch_err) = patch_status(&ctx, &name, &status).await {
                warn!(error = %patch_err, "failed to record failure status");
            }
            Err(err)
        }
    }
}

/// Delete the gateway profile before the finalizer releases the CR — but only
/// once no `OpenShellProvider` still selects this type. Deleting a profile out
/// from under live providers would break their credential handling, so the
/// finalizer refuses (surfacing [`Error::ProfileInUse`]) until they are gone.
async fn cleanup(profile: Arc<OpenShellProviderProfile>, ctx: Arc<Context>) -> Result<Action> {
    let name = profile.name_any();

    let referencing = count_referencing(&ctx, &name).await?;
    if referencing > 0 {
        return Err(refuse_in_use(&ctx, profile.as_ref(), &name, referencing).await);
    }

    info!(%name, "deleting provider profile on gateway");
    if !ctx.gateway.delete_provider_profile(&name).await? {
        info!(%name, "provider profile already absent on gateway");
    }
    Ok(Action::await_change())
}

/// Refuse to delete a profile that providers still select, returning the
/// [`Error::ProfileInUse`] that keeps the finalizer (and thus the CR) in place
/// until they are gone. Surfaced two ways: a `Warning` event and — since a
/// deleting CR lingers — the `Ready` condition, so `kubectl get` shows *why*.
async fn refuse_in_use(
    ctx: &Context,
    profile: &OpenShellProviderProfile,
    name: &str,
    count: usize,
) -> Error {
    let err = Error::ProfileInUse {
        id: name.to_owned(),
        count,
    };
    record_event(
        ctx,
        profile,
        EventType::Warning,
        err.reason(),
        "Delete",
        err.to_string(),
    )
    .await;

    let prior = profile.status.clone().unwrap_or_default();
    let mut conditions = prior.conditions.clone();
    conditions::set(
        &mut conditions,
        conditions::condition(
            conditions::READY,
            false,
            err.reason(),
            err.to_string(),
            profile.meta().generation,
            Time(chrono::Utc::now()),
        ),
    );
    let status = OpenShellProviderProfileStatus {
        conditions,
        ..prior
    };
    if let Err(patch_err) = patch_status(ctx, name, &status).await {
        warn!(error = %patch_err, "failed to record blocked-deletion status");
    }
    err
}

/// Count the providers, cluster-wide, that select the profile type `id`. Used by
/// the finalizer pre-flight; a direct list (not an informer) is fine on the rare
/// delete path. Platform profiles are shared across workspaces, so every
/// provider of this type references it regardless of its workspace.
async fn count_referencing(ctx: &Context, id: &str) -> Result<usize> {
    let providers: Api<OpenShellProvider> = Api::all(ctx.kube.clone());
    Ok(providers
        .list(&ListParams::default())
        .await?
        .into_iter()
        .filter(|provider| provider.spec.provider_type == id)
        .count())
}

async fn patch_status(
    ctx: &Context,
    name: &str,
    status: &OpenShellProviderProfileStatus,
) -> Result<()> {
    let api: Api<OpenShellProviderProfile> = Api::all(ctx.kube.clone());
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

fn error_policy(
    _profile: Arc<OpenShellProviderProfile>,
    err: &Error,
    _ctx: Arc<Context>,
) -> Action {
    warn!(error = %err, "provider profile reconcile failed; requeueing");
    // A terminal error (a malformed spec) won't clear until the spec is edited,
    // which re-triggers reconcile on its own — so back off to the normal cadence
    // instead of hot-looping (and re-emitting the same event) every 15s.
    let interval = if err.is_terminal() {
        REQUEUE_INTERVAL
    } else {
        ERROR_REQUEUE_INTERVAL
    };
    Action::requeue(interval)
}
