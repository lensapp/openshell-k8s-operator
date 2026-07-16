// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `serve`: expose the OIDC discovery document and JWKS the gateway fetches.
//!
//! Both are read from files mounted from the `ConfigMap` that `mint` published,
//! at request time, so a rotation that rewrites the `ConfigMap` is picked up
//! without a restart. This process holds no private key.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tracing::{error, info};

/// Runtime configuration, resolved from the environment the chart injects.
struct Config {
    /// Directory the JWKS `ConfigMap` is mounted into.
    config_dir: PathBuf,
    listen: String,
}

impl Config {
    fn from_env() -> Self {
        Self {
            config_dir: std::env::var("ISSUER_CONFIG_DIR")
                .unwrap_or_else(|_| "/etc/oidc".to_string())
                .into(),
            listen: std::env::var("ISSUER_LISTEN").unwrap_or_else(|_| "0.0.0.0:8081".to_string()),
        }
    }
}

pub async fn run() -> anyhow::Result<()> {
    let cfg = Arc::new(Config::from_env());
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/keys", get(keys))
        .with_state(Arc::clone(&cfg));

    let listener = tokio::net::TcpListener::bind(&cfg.listen)
        .await
        .with_context(|| format!("bind {}", cfg.listen))?;
    info!(listen = %cfg.listen, dir = %cfg.config_dir.display(), "serving OIDC discovery + JWKS");
    axum::serve(listener, app)
        .await
        .context("serve OIDC endpoints")
}

async fn discovery(State(cfg): State<Arc<Config>>) -> Response {
    serve_json(&cfg.config_dir.join("openid-configuration")).await
}

async fn keys(State(cfg): State<Arc<Config>>) -> Response {
    serve_json(&cfg.config_dir.join("jwks.json")).await
}

/// Stream a mounted JSON document back verbatim. A read failure means the
/// `ConfigMap` mount is missing or not yet populated — surface it as a 500 so
/// the gateway retries rather than caching an empty JWKS.
async fn serve_json(path: &Path) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(err) => {
            error!(path = %path.display(), %err, "failed to read OIDC document");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "issuer document unavailable",
            )
                .into_response()
        }
    }
}
