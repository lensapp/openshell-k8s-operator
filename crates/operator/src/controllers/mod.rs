// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Resource controllers.
//!
//! Each submodule owns the reconcile loop for one custom resource. [`run`]
//! starts them all concurrently over a shared [`Context`].

use std::sync::Arc;
use std::time::Duration;

use kube::Client;
use kube::Resource;
use kube::runtime::events::{Event, EventType, Recorder, Reporter};
use tracing::warn;

use crate::error::{Error, Result};
use crate::gateway::Gateway;

pub mod policy;
pub mod provider;
pub mod sandbox;
pub mod workspace;

/// Requeue interval for a successful reconcile (drift re-check cadence).
const REQUEUE_INTERVAL: Duration = Duration::from_secs(300);
/// Requeue interval while a resource is still settling on the gateway (e.g. a
/// sandbox in `Provisioning`). Short, so `.status` tracks async gateway
/// transitions promptly instead of waiting out the full drift cadence.
const TRANSITIONAL_REQUEUE_INTERVAL: Duration = Duration::from_secs(10);
/// Requeue interval after a failed reconcile.
const ERROR_REQUEUE_INTERVAL: Duration = Duration::from_secs(15);

/// Name this operator reports as when publishing Kubernetes events.
const CONTROLLER_NAME: &str = "openshell-operator";

/// State shared by every controller.
pub struct Context {
    /// Kubernetes API client.
    pub kube: Client,
    /// OpenShell gateway control-plane client.
    pub gateway: Arc<dyn Gateway>,
    /// Publishes Kubernetes events against reconciled objects.
    pub recorder: Recorder,
}

/// Run every resource controller concurrently until the process stops.
pub async fn run(kube: Client, gateway: Arc<dyn Gateway>) -> Result<()> {
    let recorder = Recorder::new(
        kube.clone(),
        Reporter {
            controller: CONTROLLER_NAME.to_owned(),
            instance: None,
        },
    );
    let context = Arc::new(Context {
        kube,
        gateway,
        recorder,
    });
    tokio::join!(
        sandbox::run(context.clone()),
        provider::run(context.clone()),
        policy::run(context.clone()),
        workspace::run(context),
    );
    Ok(())
}

/// Publish an event against `object`. Best-effort — a failure to publish is
/// logged, never propagated, so it can't mask the underlying reconcile outcome.
///
/// Conditions are the durable source of truth; events are the transient
/// breadcrumb visible in `kubectl describe`.
async fn record_event<K>(
    ctx: &Context,
    object: &K,
    type_: EventType,
    reason: &str,
    action: &str,
    note: String,
) where
    K: Resource<DynamicType = ()> + Sync,
{
    let event = Event {
        type_,
        reason: reason.to_owned(),
        note: Some(note),
        action: action.to_owned(),
        secondary: None,
    };
    if let Err(publish_err) = ctx.recorder.publish(&event, &object.object_ref(&())).await {
        warn!(error = %publish_err, "failed to publish event");
    }
}

/// Publish a `Warning` event describing a reconcile failure on `object`.
async fn record_failure<K>(ctx: &Context, object: &K, action: &str, err: &Error)
where
    K: Resource<DynamicType = ()> + Sync,
{
    record_event(
        ctx,
        object,
        EventType::Warning,
        err.reason(),
        action,
        err.to_string(),
    )
    .await;
}
