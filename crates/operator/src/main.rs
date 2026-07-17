// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Operator entrypoint: wire tracing, the gateway client, and the Kubernetes
//! client, then run the controller.

use std::sync::Arc;

use kube::Client;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, prelude::*};

use openshell_operator::gateway::{GatewayConfig, SdkGateway};
use openshell_operator::health::Health;
use openshell_operator::{controllers, health, leader};

/// Address the liveness/readiness probe server listens on.
const DEFAULT_HEALTH_LISTEN: &str = "0.0.0.0:8080";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Start the probe server first so liveness answers while the clients below
    // are still connecting. Readiness stays false until startup completes. Bind
    // here so a port conflict fails fast with a clear cause; only the accept loop
    // is detached.
    let health = Health::default();
    let listen = std::env::var("OPENSHELL_HEALTH_LISTEN")
        .unwrap_or_else(|_| DEFAULT_HEALTH_LISTEN.to_owned());
    let health_listener = health::bind(&listen).await?;
    tokio::spawn({
        let health = health.clone();
        async move {
            if let Err(err) = health::serve(health_listener, health).await {
                error!(%err, "health probe server stopped");
            }
        }
    });

    let config = GatewayConfig::from_env()?;
    info!(
        endpoint = %config.endpoint,
        authenticated = config.token.is_some(),
        "connecting to OpenShell gateway"
    );
    let gateway = Arc::new(SdkGateway::connect(config).await?);

    let kube = Client::try_default().await?;

    // Clients are built — this replica is ready to work (or to stand by).
    health.mark_ready();

    // With leader election configured, only the replica holding the lease runs
    // the controllers; losing it returns an error so this process exits and
    // Kubernetes restarts it as a standby. Unconfigured (a bare `cargo run` or a
    // single-replica install), run the controllers directly.
    if let Some(election) = leader::Config::from_env() {
        leader::run(kube.clone(), election, || controllers::run(kube, gateway)).await?;
    } else {
        info!("leader election not configured; starting controllers");
        controllers::run(kube, gateway).await?;
    }

    Ok(())
}
