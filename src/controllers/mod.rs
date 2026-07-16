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

/// Requeue interval for a successful reconcile (drift re-check cadence).
const REQUEUE_INTERVAL: Duration = Duration::from_secs(300);
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
        policy::run(context),
    );
    Ok(())
}

/// Publish a `Warning` event describing a reconcile failure on `object`.
///
/// Conditions are the durable source of truth; this adds the transient
/// breadcrumb visible in `kubectl describe`. Best-effort — a failure to publish
/// is logged, never propagated, so it can't mask the underlying reconcile error.
async fn record_failure<K>(ctx: &Context, object: &K, action: &str, err: &Error)
where
    K: Resource<DynamicType = ()> + Sync,
{
    let event = Event {
        type_: EventType::Warning,
        reason: err.reason().to_owned(),
        note: Some(err.to_string()),
        action: action.to_owned(),
        secondary: None,
    };
    if let Err(publish_err) = ctx.recorder.publish(&event, &object.object_ref(&())).await {
        warn!(error = %publish_err, "failed to publish failure event");
    }
}
