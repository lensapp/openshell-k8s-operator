// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconciliation loop for [`OpenShellPolicy`].
//!
//! An `OpenShellPolicy` owns no gateway state: it is a reusable document that
//! `OpenShellSandbox.spec.policyRef` applies at sandbox creation. This loop
//! only validates the document against the gateway's parser and mirrors the
//! result to `.status`, so authors get fast feedback on a bad policy without
//! having to create a sandbox. There is no finalizer (nothing to clean up) and
//! no gateway call.
//!
//! A sandbox blocked on a missing or invalid `OpenShellPolicy` recovers on its own: its
//! reconcile fails and is requeued, so it picks up the `OpenShellPolicy` once fixed —
//! no cross-controller triggering is needed here.

use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{
    Api, Resource, ResourceExt,
    api::{Patch, PatchParams},
    runtime::{Controller, controller::Action, watcher},
};
use tracing::{info, warn};

use super::{Context, ERROR_REQUEUE_INTERVAL, REQUEUE_INTERVAL, record_failure};
use crate::crd::{OpenShellPolicy, OpenShellPolicyStatus};
use crate::error::{Error, Result};
use crate::{conditions, policy};

/// Run the policy controller until the process is stopped.
pub async fn run(ctx: Arc<Context>) {
    let policies: Api<OpenShellPolicy> = Api::all(ctx.kube.clone());

    Controller::new(policies, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((obj, _)) => info!(policy = %obj.name, "reconciled"),
                Err(err) => warn!(error = %err, "policy reconcile loop error"),
            }
        })
        .await;
}

async fn reconcile(policy: Arc<OpenShellPolicy>, ctx: Arc<Context>) -> Result<Action> {
    let name = policy.name_any();
    let namespace = policy.namespace().ok_or(Error::MissingNamespace)?;
    info!(%name, %namespace, "validating OpenShellPolicy");

    // A rejected document is a user error, not a transient failure: record it
    // on the Ready condition and requeue normally. Editing the document bumps
    // `.metadata.generation`, which triggers a fresh reconcile — retrying an
    // unchanged bad policy would not help.
    let generation = policy.meta().generation;
    let now = Time(chrono::Utc::now());
    let ready = match policy::to_proto(&policy.spec) {
        Ok(_) => conditions::condition(
            conditions::READY,
            true,
            "Reconciled",
            "policy document is valid",
            generation,
            now,
        ),
        Err(err) => {
            warn!(%name, error = %err, "policy is invalid");
            record_failure(&ctx, policy.as_ref(), "Validate", &err).await;
            conditions::condition(
                conditions::READY,
                false,
                err.reason(),
                err.to_string(),
                generation,
                now,
            )
        }
    };

    let mut current = policy.status.clone().unwrap_or_default().conditions;
    conditions::set(&mut current, ready);
    let status = OpenShellPolicyStatus {
        conditions: current,
        observed_generation: generation,
    };
    patch_status(&ctx, &namespace, &name, &status).await?;
    Ok(Action::requeue(REQUEUE_INTERVAL))
}

async fn patch_status(
    ctx: &Context,
    namespace: &str,
    name: &str,
    status: &OpenShellPolicyStatus,
) -> Result<()> {
    let api: Api<OpenShellPolicy> = Api::namespaced(ctx.kube.clone(), namespace);
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

fn error_policy(_policy: Arc<OpenShellPolicy>, err: &Error, _ctx: Arc<Context>) -> Action {
    warn!(error = %err, "policy reconcile failed; requeueing");
    Action::requeue(ERROR_REQUEUE_INTERVAL)
}
