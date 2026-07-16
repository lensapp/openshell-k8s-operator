// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Operator entrypoint: wire tracing, the gateway client, and the Kubernetes
//! client, then run the controller.

use std::sync::Arc;

use kube::Client;
use tracing::info;
use tracing_subscriber::{EnvFilter, prelude::*};

use openshell_operator::controllers;
use openshell_operator::gateway::{GatewayConfig, SdkGateway};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = GatewayConfig::from_env()?;
    info!(
        endpoint = %config.endpoint,
        authenticated = config.token.is_some(),
        "connecting to OpenShell gateway"
    );
    let gateway = Arc::new(SdkGateway::connect(config).await?);

    let kube = Client::try_default().await?;
    info!("starting controllers");
    controllers::run(kube, gateway).await?;

    Ok(())
}
