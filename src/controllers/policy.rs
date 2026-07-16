// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconciliation loop for [`Policy`].
//!
//! A `Policy` owns no gateway state: it is a reusable document that
//! `OpenShellSandbox.spec.policyRef` applies at sandbox creation. This loop
//! only validates the document against the gateway's parser and mirrors the
//! result to `.status`, so authors get fast feedback on a bad policy without
//! having to create a sandbox. There is no finalizer (nothing to clean up) and
//! no gateway call.
//!
//! A sandbox blocked on a missing or invalid `Policy` recovers on its own: its
//! reconcile fails and is requeued, so it picks up the `Policy` once fixed —
//! no cross-controller triggering is needed here.

use std::sync::Arc;

use futures::StreamExt;
use kube::{
    Api, Resource, ResourceExt,
    api::{Patch, PatchParams},
    runtime::{Controller, controller::Action, watcher},
};
use tracing::{info, warn};

use super::{Context, ERROR_REQUEUE_INTERVAL, REQUEUE_INTERVAL};
use crate::crd::{Policy, PolicyStatus};
use crate::error::{Error, Result};
use crate::policy;

/// Run the policy controller until the process is stopped.
pub async fn run(ctx: Arc<Context>) {
    let policies: Api<Policy> = Api::all(ctx.kube.clone());

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

async fn reconcile(policy: Arc<Policy>, ctx: Arc<Context>) -> Result<Action> {
    let name = policy.name_any();
    let namespace = policy.namespace().ok_or(Error::MissingNamespace)?;
    info!(%name, %namespace, "validating Policy");

    // A rejected document is a user error, not a transient failure: record it
    // and requeue normally. Editing the document bumps `.metadata.generation`,
    // which triggers a fresh reconcile — retrying an unchanged bad policy would
    // not help.
    let status = match policy::to_proto(&policy.spec) {
        Ok(_) => PolicyStatus {
            valid: Some(true),
            message: None,
            observed_generation: policy.meta().generation,
        },
        Err(err) => {
            warn!(%name, error = %err, "policy is invalid");
            PolicyStatus {
                valid: Some(false),
                message: Some(err.to_string()),
                observed_generation: policy.meta().generation,
            }
        }
    };

    patch_status(&ctx, &namespace, &name, &status).await?;
    Ok(Action::requeue(REQUEUE_INTERVAL))
}

async fn patch_status(
    ctx: &Context,
    namespace: &str,
    name: &str,
    status: &PolicyStatus,
) -> Result<()> {
    let api: Api<Policy> = Api::namespaced(ctx.kube.clone(), namespace);
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

fn error_policy(_policy: Arc<Policy>, err: &Error, _ctx: Arc<Context>) -> Action {
    warn!(error = %err, "policy reconcile failed; requeueing");
    Action::requeue(ERROR_REQUEUE_INTERVAL)
}
