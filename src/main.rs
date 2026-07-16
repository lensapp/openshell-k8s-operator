// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Operator entrypoint: wire tracing, the gateway client, and the Kubernetes
//! client, then run the controller.

use std::sync::Arc;

use kube::Client;
use tracing::info;
use tracing_subscriber::{EnvFilter, prelude::*};

use openshell_operator::controller;
use openshell_operator::gateway::SdkGateway;

/// Gateway endpoint. Defaults to the co-located loopback gateway.
const DEFAULT_GATEWAY_ENDPOINT: &str = "http://127.0.0.1:8080";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let endpoint = std::env::var("OPENSHELL_GATEWAY_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_GATEWAY_ENDPOINT.to_string());
    info!(%endpoint, "connecting to OpenShell gateway");
    let gateway = Arc::new(SdkGateway::connect(endpoint).await?);

    let kube = Client::try_default().await?;
    info!("starting controller");
    controller::run(kube, gateway).await?;

    Ok(())
}
