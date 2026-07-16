// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Resource controllers.
//!
//! Each submodule owns the reconcile loop for one custom resource. [`run`]
//! starts them all concurrently over a shared [`Context`].

use std::sync::Arc;
use std::time::Duration;

use kube::Client;

use crate::error::Result;
use crate::gateway::Gateway;

pub mod policy;
pub mod provider;
pub mod sandbox;

/// Requeue interval for a successful reconcile (drift re-check cadence).
const REQUEUE_INTERVAL: Duration = Duration::from_secs(300);
/// Requeue interval after a failed reconcile.
const ERROR_REQUEUE_INTERVAL: Duration = Duration::from_secs(15);

/// State shared by every controller.
pub struct Context {
    /// Kubernetes API client.
    pub kube: Client,
    /// OpenShell gateway control-plane client.
    pub gateway: Arc<dyn Gateway>,
}

/// Run every resource controller concurrently until the process stops.
pub async fn run(kube: Client, gateway: Arc<dyn Gateway>) -> Result<()> {
    let context = Arc::new(Context { kube, gateway });
    tokio::join!(
        sandbox::run(context.clone()),
        provider::run(context.clone()),
        policy::run(context),
    );
    Ok(())
}
